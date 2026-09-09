//! Bounded, command-oriented persistent shell processes.
//!
//! A shell managed here is a real long-lived local shell process (`sh`/`bash`
//! on Unix or configured PowerShell on Windows). Commands are serialized,
//! stdout/stderr are retained in bounded ring buffers, and command completion
//! plus stream-drain boundaries use transport-private control framing. This
//! crate deliberately does not provide a PTY, raw input, resize, or terminal
//! byte-stream API.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
#[cfg(any(unix, windows))]
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[cfg(windows)]
mod windows;

#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::process::{Child, ChildStdin, Command, Stdio};

/// Reserved descriptor numbers used by the persistent-shell protocol on both
/// platforms. They are plain integers (not `RawFd`, which does not exist on
/// Windows) so the shell wrapper templates and any platform code can share them.
pub const STDOUT_SYNC_FD: i32 = 7;
pub const STDERR_SYNC_FD: i32 = 8;
/// Private control-channel descriptor number, used only by the Unix local
/// shell protocol (and the shell wrapper templates it emits).
#[cfg(unix)]
const CONTROL_FD: i32 = 9;
pub const CONTROL_MAGIC: &[u8] = b"WCPS1";
pub const STDOUT_SYNC_MAGIC: &[u8] = b"WCPSO1";
pub const STDERR_SYNC_MAGIC: &[u8] = b"WCPSE1";
#[cfg(any(unix, windows))]
const CONTROL_FIELD_MAX_BYTES: usize = 8 * 1024;
#[cfg(unix)]
const CONTROL_CHANNEL_CAPACITY: usize = 2;
#[cfg(any(unix, windows))]
const OUTPUT_SYNC_CHANNEL_CAPACITY: usize = 2;
#[cfg(any(unix, windows))]
const OUTPUT_READ_SLEEP: Duration = Duration::from_millis(5);
#[cfg(any(unix, windows))]
const PROCESS_SIGNAL_GRACE: Duration = Duration::from_millis(100);
const TIMEOUT_RECOVERY_WINDOW: Duration = Duration::from_millis(750);
const OPEN_INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_TERMINAL_RECORDS: usize = 128;
const MIN_OUTPUT_BYTES: usize = 1024;

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellIdentity {
    pub shell_id: String,
    pub workflow_session_id: String,
    pub runtime_project_id: String,
    pub executor: String,
    pub client_id: Option<String>,
}

/// Opaque transport-binding metadata captured at open time and preserved for
/// the life of one shell entry.
///
/// A transport reports whatever immutable facts its bindings depend on without
/// exposing transport details to the shared state machine. The Runner uses it
/// to remember the named SSH resource and its configuration generation at open,
/// so a later config change invalidates the shell instead of letting a stale
/// transport keep accepting commands. Nothing host-, credential-, or
/// ControlPath-shaped is ever stored here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransportMetadata {
    /// Named resource the transport is bound to, if any.
    pub resource: Option<String>,
    /// Configuration generation the transport was opened against, if any.
    pub generation: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ShellLaunch {
    pub identity: ShellIdentity,
    pub dialect: String,
    pub profile: Option<String>,
    pub program: String,
    /// Startup arguments owned by the selected shell/profile before any
    /// transport-specific payload mode. Unix command-mode `-c` is forbidden;
    /// Windows receives the configured PowerShell prefix arguments and the
    /// transport appends its private `-File` bootstrap.
    pub args: Vec<String>,
    pub initial_cwd: PathBuf,
    pub env: HashMap<String, String>,
    /// Initialization text evaluated once in the new shell before `open`
    /// succeeds. Its output is drained but never returned to later commands.
    pub initialization: Option<String>,
    pub max_output_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellState {
    Opening,
    Running,
    Exited,
    Closed,
    Poisoned,
    Lost,
}

impl ShellState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Opening => "opening",
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Closed => "closed",
            Self::Poisoned => "poisoned",
            Self::Lost => "lost",
        }
    }

    fn is_active(self) -> bool {
        matches!(self, Self::Opening | Self::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSummary {
    pub identity: ShellIdentity,
    pub dialect: String,
    pub profile: Option<String>,
    pub initial_cwd: PathBuf,
    pub cwd: PathBuf,
    pub created_at: i64,
    pub last_activity_at: i64,
    pub state: ShellState,
    pub busy: bool,
    pub exit_code: Option<i32>,
    pub close_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellExecResult {
    pub shell_id: String,
    pub command_started: bool,
    pub command_completed: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub duration_ms: u64,
    pub execution_state: String,
    pub shell_state: ShellState,
    pub cwd: PathBuf,
    pub error_code: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCloseResult {
    pub summary: ShellSummary,
    pub already_closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellError {
    pub code: &'static str,
    pub message: String,
}

impl ShellError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ShellError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ShellError {}

#[derive(Debug, Clone, Copy)]
pub struct ShellLimits {
    pub max_shells: usize,
    pub idle_timeout: Duration,
    pub max_terminal_records: usize,
}

impl Default for ShellLimits {
    fn default() -> Self {
        Self {
            max_shells: 8,
            idle_timeout: Duration::from_secs(30 * 60),
            max_terminal_records: DEFAULT_TERMINAL_RECORDS,
        }
    }
}

/// One parsed completion control frame emitted by a shell transport.
#[derive(Debug)]
pub struct ControlFrame {
    pub token: String,
    pub status: i32,
    pub cwd: PathBuf,
}

/// Accumulated synchronization evidence for one in-flight command. A transport
/// fills `control`, `stdout_synced`, and `stderr_synced` as it observes the
/// per-command markers; the manager treats a frame as complete only once both
/// output streams are synced and the control frame has arrived.
#[derive(Default)]
pub struct CompletionProgress {
    pub control: Option<ControlFrame>,
    pub stdout_synced: bool,
    pub stderr_synced: bool,
}

/// Outcome of waiting for a command's completion frame.
pub enum WaitOutcome {
    Frame(ControlFrame),
    Exited(ExitStatus),
    TimedOut,
    ControlLost,
}

/// Minimal transport boundary for one long-lived shell process.
///
/// Only the mechanics that differ between a local spawned child and a remote
/// shell driven over an SSH channel are abstracted here. The
/// [`PersistentShellManager`] owns all shared semantics on top of this trait:
/// identity binding, the `ShellState` machine, the busy guard, output limits,
/// timeout recovery, poisoned/lost transitions, idle reclamation, and lifecycle
/// (`close_session` / `close_project` / `close_all`).
///
/// `write_command` feeds a command plus its per-command high-entropy `token`
/// to the shell; the transport's readers surface the matching sync markers and
/// control frame through `wait_for_completion`. `interrupt` delivers a timeout
/// signal to the shell's process group so the manager can attempt sync
/// recovery. `stdout`/`stderr` expose the bounded buffers the manager snapshots
/// per command.
pub trait ShellTransport: Send + Sync {
    fn set_expected_token(&self, token: &str);
    fn write_command(&self, command: &str, token: &str) -> Result<(), ShellError>;
    fn wait_for_completion(
        &self,
        token: &str,
        timeout: Duration,
        progress: &mut CompletionProgress,
    ) -> WaitOutcome;
    fn try_wait(&self) -> Option<ExitStatus>;
    /// Best-effort timeout interruption. The transport chooses the narrowest
    /// process-lifecycle primitive its host supports; the manager decides whether
    /// to keep or poison the shell only from subsequent synchronization evidence.
    fn interrupt(&self);
    fn shutdown(&self);
    fn terminate_remaining_group_after_exit(&self);
    fn stdout(&self) -> &Arc<Mutex<BoundedBuffer>>;
    fn stderr(&self) -> &Arc<Mutex<BoundedBuffer>>;
    /// Validate the shell-reported cwd in the transport's path namespace.
    /// Local transports use the host platform's path rules. Remote transports
    /// can override this when their shell path syntax differs from the Runner.
    fn reported_cwd_is_absolute(&self, cwd: &Path) -> bool {
        cwd.is_absolute()
    }
    /// Opaque binding metadata captured at open. The shared manager stores it
    /// on the entry so callers can validate bindings that a transport depends
    /// on (e.g. an SSH resource + config generation). The default is no
    /// metadata; only transports with a real binding override this.
    fn metadata(&self) -> Option<TransportMetadata> {
        None
    }
}

#[derive(Debug)]
pub struct BoundedBuffer {
    bytes: VecDeque<u8>,
    first_offset: u64,
    next_offset: u64,
    max_bytes: usize,
}

impl BoundedBuffer {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(max_bytes.min(8192)),
            first_offset: 0,
            next_offset: 0,
            max_bytes: max_bytes.max(MIN_OUTPUT_BYTES),
        }
    }

    pub fn append(&mut self, bytes: &[u8]) {
        self.next_offset = self
            .next_offset
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        self.bytes.extend(bytes.iter().copied());
        while self.bytes.len() > self.max_bytes {
            self.bytes.pop_front();
            self.first_offset = self.first_offset.saturating_add(1);
        }
    }

    pub fn set_max_bytes(&mut self, max_bytes: usize) {
        self.max_bytes = max_bytes.max(MIN_OUTPUT_BYTES);
        while self.bytes.len() > self.max_bytes {
            self.bytes.pop_front();
            self.first_offset = self.first_offset.saturating_add(1);
        }
    }

    pub fn cursor(&self) -> u64 {
        self.next_offset
    }

    pub fn snapshot_since(&self, requested_start: u64) -> (String, bool) {
        let start = requested_start.max(self.first_offset).min(self.next_offset);
        let skip = usize::try_from(start.saturating_sub(self.first_offset)).unwrap_or(usize::MAX);
        let retained = self.bytes.iter().skip(skip).copied().collect::<Vec<_>>();
        (
            String::from_utf8_lossy(&retained).into_owned(),
            requested_start < self.first_offset,
        )
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct ShellProcess {
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    process_group_id: u32,
    control_printf: String,
    control_pwd: String,
    expected_token: Arc<Mutex<Option<String>>>,
    control_rx: Mutex<mpsc::Receiver<ControlFrame>>,
    stdout_sync_rx: Mutex<mpsc::Receiver<String>>,
    stderr_sync_rx: Mutex<mpsc::Receiver<String>>,
    stdout: Arc<Mutex<BoundedBuffer>>,
    stderr: Arc<Mutex<BoundedBuffer>>,
    readers_stop: Arc<AtomicBool>,
    reader_threads: Mutex<Option<Vec<thread::JoinHandle<()>>>>,
    shutdown_started: AtomicBool,
}

#[cfg(unix)]
struct SpawnedChildGuard {
    child: Option<Child>,
    process_group_id: u32,
}

#[cfg(unix)]
impl SpawnedChildGuard {
    fn new(child: Child) -> Self {
        let process_group_id = child.id();
        Self {
            child: Some(child),
            process_group_id,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child
            .as_mut()
            .expect("spawned shell child guard was already disarmed")
    }

    fn disarm(mut self) -> Child {
        self.child
            .take()
            .expect("spawned shell child guard was already disarmed")
    }
}

#[cfg(unix)]
impl Drop for SpawnedChildGuard {
    fn drop(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        let _ = signal_process_group(self.process_group_id, libc::SIGTERM);
        let deadline = Instant::now() + PROCESS_SIGNAL_GRACE;
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let _ = signal_process_group(self.process_group_id, libc::SIGKILL);
        let kill_deadline = Instant::now() + PROCESS_SIGNAL_GRACE;
        while Instant::now() < kill_deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

struct ShellEntry {
    identity: ShellIdentity,
    dialect: String,
    profile: Option<String>,
    initial_cwd: Mutex<PathBuf>,
    initial_cwd_frozen: AtomicBool,
    current_cwd: Mutex<PathBuf>,
    created_at: i64,
    last_activity_at: AtomicU64,
    last_activity_instant: Mutex<Instant>,
    state: Mutex<ShellState>,
    busy: AtomicBool,
    exit_code: Mutex<Option<i32>>,
    close_reason: Mutex<Option<String>>,
    metadata: Mutex<Option<TransportMetadata>>,
    process: Box<dyn ShellTransport>,
}

impl std::fmt::Debug for ShellEntry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShellEntry")
            .field("identity", &self.identity)
            .field("dialect", &self.dialect)
            .field("profile", &self.profile)
            .field("initial_cwd", &*lock_unpoison(&self.initial_cwd))
            .field("state", &*lock_unpoison(&self.state))
            .finish_non_exhaustive()
    }
}

impl ShellEntry {
    fn summary(&self) -> ShellSummary {
        let current_cwd = lock_unpoison(&self.current_cwd).clone();
        ShellSummary {
            identity: self.identity.clone(),
            dialect: self.dialect.clone(),
            profile: self.profile.clone(),
            initial_cwd: lock_unpoison(&self.initial_cwd).clone(),
            cwd: current_cwd,
            created_at: self.created_at,
            last_activity_at: self.last_activity_at.load(Ordering::SeqCst) as i64,
            state: *lock_unpoison(&self.state),
            busy: self.busy.load(Ordering::SeqCst),
            exit_code: *lock_unpoison(&self.exit_code),
            close_reason: lock_unpoison(&self.close_reason).clone(),
        }
    }

    fn touch(&self) {
        self.last_activity_at
            .store(now_ts().max(0) as u64, Ordering::SeqCst);
        *lock_unpoison(&self.last_activity_instant) = Instant::now();
    }

    fn validate_identity(
        &self,
        workflow_session_id: &str,
        runtime_project_id: &str,
    ) -> Result<(), ShellError> {
        if self.identity.workflow_session_id != workflow_session_id
            || self.identity.runtime_project_id != runtime_project_id
        {
            return Err(ShellError::new(
                "persistent_shell_not_found",
                "persistent shell does not belong to the requested Workflow Session and project",
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct ManagerInner {
    entries: Mutex<HashMap<String, Arc<ShellEntry>>>,
    active_by_session: Mutex<HashMap<String, String>>,
    terminal_order: Mutex<VecDeque<String>>,
    max_shells: AtomicUsize,
    idle_timeout_secs: AtomicU64,
    max_terminal_records: AtomicUsize,
    sweeper_started: AtomicBool,
    stop_sweeper: AtomicBool,
}

impl Drop for ManagerInner {
    fn drop(&mut self) {
        self.stop_sweeper.store(true, Ordering::SeqCst);
        let entries = lock_unpoison(&self.entries)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for entry in entries {
            entry.process.shutdown();
        }
    }
}

#[derive(Debug, Clone)]
pub struct PersistentShellManager {
    inner: Arc<ManagerInner>,
}

impl PersistentShellManager {
    pub fn new(limits: ShellLimits) -> Self {
        let inner = Arc::new(ManagerInner {
            entries: Mutex::new(HashMap::new()),
            active_by_session: Mutex::new(HashMap::new()),
            terminal_order: Mutex::new(VecDeque::new()),
            max_shells: AtomicUsize::new(limits.max_shells.max(1)),
            idle_timeout_secs: AtomicU64::new(limits.idle_timeout.as_secs().max(1)),
            max_terminal_records: AtomicUsize::new(limits.max_terminal_records.max(1)),
            sweeper_started: AtomicBool::new(false),
            stop_sweeper: AtomicBool::new(false),
        });
        Self { inner }
    }

    pub fn update_limits(&self, limits: ShellLimits) {
        self.inner
            .max_shells
            .store(limits.max_shells.max(1), Ordering::SeqCst);
        self.inner
            .idle_timeout_secs
            .store(limits.idle_timeout.as_secs().max(1), Ordering::SeqCst);
        self.inner
            .max_terminal_records
            .store(limits.max_terminal_records.max(1), Ordering::SeqCst);
        self.prune_terminal_records();
    }

    pub fn open(&self, launch: ShellLaunch) -> Result<ShellSummary, ShellError> {
        ensure_local_shell_supported()?;
        validate_launch(&launch)?;
        self.ensure_idle_sweeper();
        self.sweep_idle();
        {
            let entries = lock_unpoison(&self.inner.entries);
            if entries.contains_key(&launch.identity.shell_id) {
                return Err(ShellError::new(
                    "persistent_shell_id_conflict",
                    "persistent shell id already exists",
                ));
            }
            let active = entries
                .values()
                .filter(|entry| lock_unpoison(&entry.state).is_active())
                .count();
            if active >= self.inner.max_shells.load(Ordering::SeqCst) {
                return Err(ShellError::new(
                    "persistent_shell_limit_reached",
                    format!(
                        "persistent shell limit reached ({})",
                        self.inner.max_shells.load(Ordering::SeqCst)
                    ),
                ));
            }
        }
        {
            let active = lock_unpoison(&self.inner.active_by_session);
            if active.contains_key(&launch.identity.workflow_session_id) {
                return Err(ShellError::new(
                    "persistent_shell_already_open",
                    "Workflow Session already has an active persistent shell",
                ));
            }
        }

        #[cfg(unix)]
        let process: Box<dyn ShellTransport> = Box::new(spawn_shell_process(&launch)?);
        #[cfg(windows)]
        let process: Box<dyn ShellTransport> = windows::spawn_shell_process(&launch)?;
        #[cfg(not(any(unix, windows)))]
        let process: Box<dyn ShellTransport> = spawn_shell_process(&launch)?;
        let timestamp = now_ts();

        let entry = Arc::new(ShellEntry {
            identity: launch.identity.clone(),
            dialect: launch.dialect,
            profile: launch.profile,
            initial_cwd: Mutex::new(launch.initial_cwd.clone()),
            initial_cwd_frozen: AtomicBool::new(true),
            current_cwd: Mutex::new(launch.initial_cwd),
            created_at: timestamp,
            last_activity_at: AtomicU64::new(timestamp.max(0) as u64),
            last_activity_instant: Mutex::new(Instant::now()),
            state: Mutex::new(ShellState::Opening),
            busy: AtomicBool::new(false),
            exit_code: Mutex::new(None),
            close_reason: Mutex::new(None),
            metadata: Mutex::new(process.metadata()),
            process,
        });

        let initialization = launch.initialization.unwrap_or_default();
        self.register_and_initialize(&entry, &initialization)
    }

    /// Open a persistent shell backed by an externally-provided transport
    /// (e.g. a remote SSH shell). The caller is responsible for spawning the
    /// transport and for the remote cwd/bootstrap; this method runs the same
    /// registration, initialization, and synchronization logic as [`open`], so
    /// the shared state machine, limits, idle sweeper, and lifecycle apply
    /// unchanged. `initial_cwd_seed` is provisional opening state only. A
    /// successful first control frame replaces it with the authoritative,
    /// absolute cwd reported by the transport and freezes that value.
    pub fn open_with_transport(
        &self,
        identity: ShellIdentity,
        dialect: String,
        profile: Option<String>,
        initial_cwd_seed: PathBuf,
        initialization: Option<String>,
        transport: Box<dyn ShellTransport>,
    ) -> Result<ShellSummary, ShellError> {
        self.ensure_idle_sweeper();
        self.sweep_idle();
        {
            let entries = lock_unpoison(&self.inner.entries);
            if entries.contains_key(&identity.shell_id) {
                return Err(ShellError::new(
                    "persistent_shell_id_conflict",
                    "persistent shell id already exists",
                ));
            }
            let active = entries
                .values()
                .filter(|entry| lock_unpoison(&entry.state).is_active())
                .count();
            if active >= self.inner.max_shells.load(Ordering::SeqCst) {
                return Err(ShellError::new(
                    "persistent_shell_limit_reached",
                    format!(
                        "persistent shell limit reached ({})",
                        self.inner.max_shells.load(Ordering::SeqCst)
                    ),
                ));
            }
        }
        {
            let active = lock_unpoison(&self.inner.active_by_session);
            if active.contains_key(&identity.workflow_session_id) {
                return Err(ShellError::new(
                    "persistent_shell_already_open",
                    "Workflow Session already has an active persistent shell",
                ));
            }
        }

        let timestamp = now_ts();
        let entry = Arc::new(ShellEntry {
            identity,
            dialect,
            profile,
            initial_cwd: Mutex::new(initial_cwd_seed.clone()),
            initial_cwd_frozen: AtomicBool::new(false),
            current_cwd: Mutex::new(initial_cwd_seed),
            created_at: timestamp,
            last_activity_at: AtomicU64::new(timestamp.max(0) as u64),
            last_activity_instant: Mutex::new(Instant::now()),
            state: Mutex::new(ShellState::Opening),
            busy: AtomicBool::new(false),
            exit_code: Mutex::new(None),
            close_reason: Mutex::new(None),
            metadata: Mutex::new(transport.metadata()),
            process: transport,
        });

        let initialization = initialization.unwrap_or_default();
        self.register_and_initialize(&entry, &initialization)
    }

    fn register_and_initialize(
        &self,
        entry: &Arc<ShellEntry>,
        initialization: &str,
    ) -> Result<ShellSummary, ShellError> {
        {
            let mut entries = lock_unpoison(&self.inner.entries);
            let mut active = lock_unpoison(&self.inner.active_by_session);
            if active.contains_key(&entry.identity.workflow_session_id) {
                entry.process.shutdown();
                return Err(ShellError::new(
                    "persistent_shell_already_open",
                    "Workflow Session already has an active persistent shell",
                ));
            }
            let active_count = entries
                .values()
                .filter(|existing| lock_unpoison(&existing.state).is_active())
                .count();
            if active_count >= self.inner.max_shells.load(Ordering::SeqCst) {
                entry.process.shutdown();
                return Err(ShellError::new(
                    "persistent_shell_limit_reached",
                    format!(
                        "persistent shell limit reached ({})",
                        self.inner.max_shells.load(Ordering::SeqCst)
                    ),
                ));
            }
            active.insert(
                entry.identity.workflow_session_id.clone(),
                entry.identity.shell_id.clone(),
            );
            entries.insert(entry.identity.shell_id.clone(), Arc::clone(entry));
        }

        let init_token = command_token();
        entry.process.set_expected_token(&init_token);
        let mut completion = CompletionProgress::default();
        if let Err(error) = entry.process.write_command(initialization, &init_token) {
            self.transition_terminal(
                entry,
                ShellState::Poisoned,
                None,
                Some("initialization_write_failed".to_string()),
            );
            entry.process.shutdown();
            return Err(error);
        }
        match entry.process.wait_for_completion(
            &init_token,
            OPEN_INITIALIZATION_TIMEOUT,
            &mut completion,
        ) {
            WaitOutcome::Frame(frame) if frame.status == 0 => {
                if !entry.process.reported_cwd_is_absolute(&frame.cwd) {
                    self.transition_terminal(
                        entry,
                        ShellState::Poisoned,
                        None,
                        Some("initialization_cwd_unobservable".to_string()),
                    );
                    entry.process.shutdown();
                    return Err(ShellError::new(
                        "shell_reset_required",
                        "persistent shell initialization did not report an absolute cwd",
                    ));
                }
                let promoted = {
                    let mut state = lock_unpoison(&entry.state);
                    if *state == ShellState::Opening {
                        *state = ShellState::Running;
                        true
                    } else {
                        false
                    }
                };
                if !promoted {
                    entry.process.shutdown();
                    return Err(ShellError::new(
                        "persistent_shell_stale",
                        "persistent shell was closed while it was opening",
                    ));
                }
                let authoritative_cwd = frame.cwd;
                {
                    let mut initial_cwd = lock_unpoison(&entry.initial_cwd);
                    if entry
                        .initial_cwd_frozen
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        *initial_cwd = authoritative_cwd.clone();
                    }
                }
                *lock_unpoison(&entry.current_cwd) = authoritative_cwd;
                entry.touch();
                Ok(entry.summary())
            }
            WaitOutcome::Frame(frame) => {
                self.transition_terminal(
                    entry,
                    ShellState::Poisoned,
                    Some(frame.status),
                    Some("initialization_failed".to_string()),
                );
                entry.process.shutdown();
                Err(ShellError::new(
                    "persistent_shell_initialization_failed",
                    format!(
                        "persistent shell initialization exited with {}",
                        frame.status
                    ),
                ))
            }
            WaitOutcome::Exited(status) => {
                let code = status.code();
                self.transition_terminal(
                    entry,
                    ShellState::Exited,
                    code,
                    Some("shell_exited_during_initialization".to_string()),
                );
                entry.process.shutdown();
                Err(ShellError::new(
                    "persistent_shell_exited",
                    "persistent shell exited during initialization",
                ))
            }
            WaitOutcome::TimedOut | WaitOutcome::ControlLost => {
                self.transition_terminal(
                    entry,
                    ShellState::Poisoned,
                    None,
                    Some("initialization_sync_lost".to_string()),
                );
                entry.process.shutdown();
                Err(ShellError::new(
                    "shell_reset_required",
                    "persistent shell initialization did not reach a synchronized state",
                ))
            }
        }
    }

    pub fn exec(
        &self,
        shell_id: &str,
        workflow_session_id: &str,
        runtime_project_id: &str,
        command: &str,
        timeout: Duration,
    ) -> Result<ShellExecResult, ShellError> {
        if command.contains('\0') {
            return Err(ShellError::new(
                "persistent_shell_invalid_command",
                "command cannot contain NUL bytes",
            ));
        }
        if timeout.is_zero() {
            return Err(ShellError::new(
                "persistent_shell_invalid_timeout",
                "timeout must be greater than zero",
            ));
        }
        self.sweep_idle();
        let entry = self.lookup(shell_id, workflow_session_id, runtime_project_id)?;
        self.refresh_exit(&entry);
        let state = *lock_unpoison(&entry.state);
        if state == ShellState::Opening {
            return Err(ShellError::new(
                "shell_busy",
                "persistent shell is still opening",
            ));
        }
        if state != ShellState::Running {
            return Err(ShellError::new(
                "persistent_shell_stale",
                format!("persistent shell is {}", state.as_str()),
            ));
        }
        if entry
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(ShellError::new(
                "shell_busy",
                "persistent shell is already executing a command",
            ));
        }
        if *lock_unpoison(&entry.state) != ShellState::Running {
            entry.busy.store(false, Ordering::SeqCst);
            return Err(ShellError::new(
                "persistent_shell_stale",
                "persistent shell became unavailable before command dispatch",
            ));
        }
        let _busy = BusyGuard {
            entry: Arc::clone(&entry),
        };
        entry.touch();

        let stdout_start = lock_unpoison(entry.process.stdout()).cursor();
        let stderr_start = lock_unpoison(entry.process.stderr()).cursor();
        let token = command_token();
        entry.process.set_expected_token(&token);
        let mut completion = CompletionProgress::default();
        let started = Instant::now();
        if let Err(error) = entry.process.write_command(command, &token) {
            self.transition_terminal(
                &entry,
                ShellState::Poisoned,
                None,
                Some("command_write_failed".to_string()),
            );
            entry.process.shutdown();
            return Err(error);
        }

        let outcome = entry
            .process
            .wait_for_completion(&token, timeout, &mut completion);
        let mut timed_out = false;
        let resolved = match outcome {
            WaitOutcome::TimedOut => {
                timed_out = true;
                entry.process.interrupt();
                entry
                    .process
                    .wait_for_completion(&token, TIMEOUT_RECOVERY_WINDOW, &mut completion)
            }
            other => other,
        };
        if timed_out && !matches!(resolved, WaitOutcome::Frame(_) | WaitOutcome::Exited(_)) {
            self.transition_terminal(
                &entry,
                ShellState::Poisoned,
                None,
                Some("command_timeout_sync_lost".to_string()),
            );
            entry.process.shutdown();
            let (stdout, stdout_truncated) =
                lock_unpoison(entry.process.stdout()).snapshot_since(stdout_start);
            let (stderr, stderr_truncated) =
                lock_unpoison(entry.process.stderr()).snapshot_since(stderr_start);
            entry.touch();
            return Ok(ShellExecResult {
                shell_id: shell_id.to_string(),
                command_started: true,
                command_completed: false,
                exit_code: None,
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
                duration_ms: started.elapsed().as_millis() as u64,
                execution_state: "timed_out".to_string(),
                shell_state: ShellState::Poisoned,
                cwd: lock_unpoison(&entry.current_cwd).clone(),
                error_code: Some("shell_reset_required".to_string()),
                error: Some(
                    "command timed out and persistent shell synchronization could not be recovered"
                        .to_string(),
                ),
            });
        }

        match &resolved {
            WaitOutcome::Exited(_) => entry.process.terminate_remaining_group_after_exit(),
            WaitOutcome::ControlLost | WaitOutcome::TimedOut => entry.process.shutdown(),
            WaitOutcome::Frame(_) => {}
        }
        let (stdout, stdout_truncated) =
            lock_unpoison(entry.process.stdout()).snapshot_since(stdout_start);
        let (stderr, stderr_truncated) =
            lock_unpoison(entry.process.stderr()).snapshot_since(stderr_start);
        let duration_ms = started.elapsed().as_millis() as u64;
        entry.touch();

        match resolved {
            WaitOutcome::Frame(frame) => {
                if !entry.process.reported_cwd_is_absolute(&frame.cwd) {
                    self.transition_terminal(
                        &entry,
                        ShellState::Poisoned,
                        None,
                        Some("command_cwd_unobservable".to_string()),
                    );
                    entry.process.shutdown();
                    return Ok(ShellExecResult {
                        shell_id: shell_id.to_string(),
                        command_started: true,
                        command_completed: true,
                        exit_code: Some(frame.status),
                        stdout,
                        stderr,
                        stdout_truncated,
                        stderr_truncated,
                        duration_ms,
                        execution_state: "lost".to_string(),
                        shell_state: ShellState::Poisoned,
                        cwd: lock_unpoison(&entry.current_cwd).clone(),
                        error_code: Some("shell_reset_required".to_string()),
                        error: Some(
                            "persistent shell cwd could not be observed safely; reopen the shell"
                                .to_string(),
                        ),
                    });
                }
                let shell_state = {
                    let mut state = lock_unpoison(&entry.state);
                    if state.is_active() {
                        *state = ShellState::Running;
                    }
                    *state
                };
                if shell_state == ShellState::Running {
                    *lock_unpoison(&entry.current_cwd) = frame.cwd;
                }
                let interrupted = shell_state != ShellState::Running;
                Ok(ShellExecResult {
                    shell_id: shell_id.to_string(),
                    command_started: true,
                    command_completed: true,
                    exit_code: Some(frame.status),
                    stdout,
                    stderr,
                    stdout_truncated,
                    stderr_truncated,
                    duration_ms,
                    execution_state: if timed_out {
                        "timed_out".to_string()
                    } else if interrupted {
                        "interrupted".to_string()
                    } else {
                        "completed".to_string()
                    },
                    shell_state,
                    cwd: lock_unpoison(&entry.current_cwd).clone(),
                    error_code: if interrupted {
                        Some("shell_reset_required".to_string())
                    } else {
                        timed_out.then(|| "command_timeout".to_string())
                    },
                    error: if interrupted {
                        Some(
                            "persistent shell was closed while the command was completing"
                                .to_string(),
                        )
                    } else {
                        timed_out.then(|| {
                            "command timed out but the persistent shell recovered synchronization"
                                .to_string()
                        })
                    },
                })
            }
            WaitOutcome::Exited(status) => {
                let code = status.code();
                self.transition_terminal(
                    &entry,
                    ShellState::Exited,
                    code,
                    Some(if timed_out {
                        "shell_exited_after_timeout".to_string()
                    } else {
                        "shell_process_exited".to_string()
                    }),
                );
                entry.process.terminate_remaining_group_after_exit();
                Ok(ShellExecResult {
                    shell_id: shell_id.to_string(),
                    command_started: true,
                    // A normal shell exit is an authoritative terminal
                    // conclusion for the only command in flight, even though
                    // the shell cannot emit the control frame afterward.
                    command_completed: !timed_out,
                    exit_code: code,
                    stdout,
                    stderr,
                    stdout_truncated,
                    stderr_truncated,
                    duration_ms,
                    execution_state: if timed_out {
                        "timed_out".to_string()
                    } else {
                        "shell_exited".to_string()
                    },
                    shell_state: ShellState::Exited,
                    cwd: lock_unpoison(&entry.current_cwd).clone(),
                    error_code: timed_out.then(|| "shell_reset_required".to_string()),
                    error: timed_out.then(|| {
                        "command timeout caused the persistent shell to exit; reopen it".to_string()
                    }),
                })
            }
            WaitOutcome::ControlLost | WaitOutcome::TimedOut => {
                self.transition_terminal(
                    &entry,
                    ShellState::Poisoned,
                    None,
                    Some("control_channel_lost".to_string()),
                );
                entry.process.shutdown();
                Ok(ShellExecResult {
                    shell_id: shell_id.to_string(),
                    command_started: true,
                    command_completed: false,
                    exit_code: None,
                    stdout,
                    stderr,
                    stdout_truncated,
                    stderr_truncated,
                    duration_ms,
                    execution_state: "lost".to_string(),
                    shell_state: ShellState::Poisoned,
                    cwd: lock_unpoison(&entry.current_cwd).clone(),
                    error_code: Some("shell_reset_required".to_string()),
                    error: Some(
                        "persistent shell control channel was lost; reopen the shell".to_string(),
                    ),
                })
            }
        }
    }

    pub fn status(
        &self,
        shell_id: &str,
        workflow_session_id: &str,
        runtime_project_id: &str,
    ) -> Result<ShellSummary, ShellError> {
        self.sweep_idle();
        let entry = self.lookup(shell_id, workflow_session_id, runtime_project_id)?;
        self.refresh_exit(&entry);
        Ok(entry.summary())
    }

    pub fn set_output_limit(
        &self,
        shell_id: &str,
        workflow_session_id: &str,
        runtime_project_id: &str,
        max_output_bytes: usize,
    ) -> Result<(), ShellError> {
        let entry = self.lookup(shell_id, workflow_session_id, runtime_project_id)?;
        lock_unpoison(entry.process.stdout()).set_max_bytes(max_output_bytes);
        lock_unpoison(entry.process.stderr()).set_max_bytes(max_output_bytes);
        Ok(())
    }

    pub fn close(
        &self,
        shell_id: &str,
        workflow_session_id: &str,
        runtime_project_id: &str,
        reason: &str,
    ) -> Result<ShellCloseResult, ShellError> {
        let entry = self.lookup(shell_id, workflow_session_id, runtime_project_id)?;
        self.refresh_exit(&entry);
        let already_closed = {
            let state = *lock_unpoison(&entry.state);
            matches!(
                state,
                ShellState::Closed | ShellState::Exited | ShellState::Poisoned | ShellState::Lost
            )
        };
        if !already_closed {
            self.transition_terminal(&entry, ShellState::Closed, None, Some(reason.to_string()));
            entry.process.shutdown();
        }
        Ok(ShellCloseResult {
            summary: entry.summary(),
            already_closed,
        })
    }

    pub fn close_session(&self, workflow_session_id: &str, reason: &str) -> usize {
        let entries = lock_unpoison(&self.inner.entries)
            .values()
            .filter(|entry| entry.identity.workflow_session_id == workflow_session_id)
            .cloned()
            .collect::<Vec<_>>();
        let mut closed = 0;
        for entry in entries {
            self.refresh_exit(&entry);
            if lock_unpoison(&entry.state).is_active() {
                self.transition_terminal(
                    &entry,
                    ShellState::Closed,
                    None,
                    Some(reason.to_string()),
                );
                entry.process.shutdown();
                closed += 1;
            }
        }
        closed
    }

    pub fn close_project(&self, runtime_project_id: &str, reason: &str) -> usize {
        let entries = lock_unpoison(&self.inner.entries)
            .values()
            .filter(|entry| entry.identity.runtime_project_id == runtime_project_id)
            .cloned()
            .collect::<Vec<_>>();
        let mut closed = 0;
        for entry in entries {
            self.refresh_exit(&entry);
            if lock_unpoison(&entry.state).is_active() {
                self.transition_terminal(
                    &entry,
                    ShellState::Closed,
                    None,
                    Some(reason.to_string()),
                );
                entry.process.shutdown();
                closed += 1;
            }
        }
        closed
    }

    pub fn close_all(&self, reason: &str) -> usize {
        let entries = lock_unpoison(&self.inner.entries)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut closed = 0;
        for entry in entries {
            self.refresh_exit(&entry);
            if lock_unpoison(&entry.state).is_active() {
                self.transition_terminal(
                    &entry,
                    ShellState::Closed,
                    None,
                    Some(reason.to_string()),
                );
                entry.process.shutdown();
                closed += 1;
            }
        }
        closed
    }

    pub fn active_count(&self) -> usize {
        lock_unpoison(&self.inner.active_by_session).len()
    }

    /// The opaque transport binding metadata captured when the shell was
    /// opened, if the transport reported any. Used by callers to validate that
    /// the bindings an open shell depends on are still current (e.g. a named
    /// SSH resource at the configuration generation it was opened against).
    pub fn metadata(
        &self,
        shell_id: &str,
        workflow_session_id: &str,
        runtime_project_id: &str,
    ) -> Result<Option<TransportMetadata>, ShellError> {
        let entry = self.lookup(shell_id, workflow_session_id, runtime_project_id)?;
        let metadata = lock_unpoison(&entry.metadata).clone();
        Ok(metadata)
    }

    pub fn sweep_idle(&self) -> usize {
        let idle = Duration::from_secs(self.inner.idle_timeout_secs.load(Ordering::SeqCst));
        let entries = lock_unpoison(&self.inner.entries)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut closed = 0;
        for entry in entries {
            self.refresh_exit(&entry);
            let should_attempt_close = lock_unpoison(&entry.state).is_active()
                && lock_unpoison(&entry.last_activity_instant).elapsed() >= idle;
            if !should_attempt_close {
                continue;
            }
            if entry
                .busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                continue;
            }
            let should_close = lock_unpoison(&entry.state).is_active()
                && lock_unpoison(&entry.last_activity_instant).elapsed() >= idle;
            if !should_close {
                entry.busy.store(false, Ordering::SeqCst);
                continue;
            }
            self.transition_terminal(
                &entry,
                ShellState::Closed,
                None,
                Some("idle_timeout".to_string()),
            );
            entry.process.shutdown();
            closed += 1;
        }
        closed
    }

    fn lookup(
        &self,
        shell_id: &str,
        workflow_session_id: &str,
        runtime_project_id: &str,
    ) -> Result<Arc<ShellEntry>, ShellError> {
        let entry = lock_unpoison(&self.inner.entries)
            .get(shell_id)
            .cloned()
            .ok_or_else(|| {
                ShellError::new(
                    "persistent_shell_not_found",
                    "persistent shell was not found or belongs to another runtime",
                )
            })?;
        entry.validate_identity(workflow_session_id, runtime_project_id)?;
        Ok(entry)
    }

    fn ensure_idle_sweeper(&self) {
        if !self.inner.sweeper_started.swap(true, Ordering::SeqCst) {
            spawn_idle_sweeper(Arc::downgrade(&self.inner));
        }
    }

    fn refresh_exit(&self, entry: &Arc<ShellEntry>) {
        if !lock_unpoison(&entry.state).is_active() {
            return;
        }
        if let Some(status) = entry.process.try_wait() {
            self.transition_terminal(
                entry,
                ShellState::Exited,
                status.code(),
                Some("shell_process_exited".to_string()),
            );
            entry.process.terminate_remaining_group_after_exit();
        }
    }

    fn transition_terminal(
        &self,
        entry: &Arc<ShellEntry>,
        state: ShellState,
        exit_code: Option<i32>,
        reason: Option<String>,
    ) {
        {
            let mut current = lock_unpoison(&entry.state);
            if !current.is_active() && *current != ShellState::Opening {
                return;
            }
            *current = state;
        }
        entry.busy.store(false, Ordering::SeqCst);
        *lock_unpoison(&entry.exit_code) = exit_code;
        *lock_unpoison(&entry.close_reason) = reason;
        // A terminal shell can never be reused; release its transport-binding
        // metadata so a stale binding cannot keep validating a dead entry.
        *lock_unpoison(&entry.metadata) = None;
        entry.touch();
        let mut active = lock_unpoison(&self.inner.active_by_session);
        if active
            .get(&entry.identity.workflow_session_id)
            .is_some_and(|id| id == &entry.identity.shell_id)
        {
            active.remove(&entry.identity.workflow_session_id);
        }
        drop(active);
        let mut order = lock_unpoison(&self.inner.terminal_order);
        if !order.iter().any(|id| id == &entry.identity.shell_id) {
            order.push_back(entry.identity.shell_id.clone());
        }
        drop(order);
        self.prune_terminal_records();
    }

    fn prune_terminal_records(&self) {
        let limit = self.inner.max_terminal_records.load(Ordering::SeqCst);
        loop {
            let remove = {
                let mut order = lock_unpoison(&self.inner.terminal_order);
                (order.len() > limit).then(|| order.pop_front()).flatten()
            };
            let Some(shell_id) = remove else {
                break;
            };
            let mut entries = lock_unpoison(&self.inner.entries);
            if entries
                .get(&shell_id)
                .is_some_and(|entry| !lock_unpoison(&entry.state).is_active())
            {
                entries.remove(&shell_id);
            }
        }
    }
}

struct BusyGuard {
    entry: Arc<ShellEntry>,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.entry.busy.store(false, Ordering::SeqCst);
    }
}

#[cfg(unix)]
impl ShellProcess {
    fn set_expected_token(&self, token: &str) {
        *lock_unpoison(&self.expected_token) = Some(token.to_string());
    }

    fn interrupt(&self) {
        let _ = signal_process_group(self.process_group_id, libc::SIGINT);
    }

    fn stdout_buffer(&self) -> &Arc<Mutex<BoundedBuffer>> {
        &self.stdout
    }

    fn stderr_buffer(&self) -> &Arc<Mutex<BoundedBuffer>> {
        &self.stderr
    }

    fn write_command(&self, command: &str, token: &str) -> Result<(), ShellError> {
        let wrapper = command_wrapper(command, token, &self.control_printf, &self.control_pwd);
        let mut stdin = lock_unpoison(&self.stdin);
        let Some(stdin) = stdin.as_mut() else {
            return Err(ShellError::new(
                "persistent_shell_stale",
                "persistent shell stdin is closed",
            ));
        };
        stdin.write_all(wrapper.as_bytes()).map_err(|error| {
            ShellError::new(
                "persistent_shell_write_failed",
                format!("failed to write command to persistent shell: {error}"),
            )
        })?;
        stdin.flush().map_err(|error| {
            ShellError::new(
                "persistent_shell_write_failed",
                format!("failed to flush persistent shell command: {error}"),
            )
        })
    }

    fn wait_for_completion(
        &self,
        token: &str,
        timeout: Duration,
        progress: &mut CompletionProgress,
    ) -> WaitOutcome {
        let deadline = Instant::now() + timeout;
        loop {
            let control_disconnected = {
                let receiver = lock_unpoison(&self.control_rx);
                let mut disconnected = false;
                loop {
                    match receiver.try_recv() {
                        Ok(frame) if frame.token == token => progress.control = Some(frame),
                        Ok(_) => {}
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }
                disconnected
            };
            let stdout_disconnected =
                drain_sync_receiver(&self.stdout_sync_rx, token, &mut progress.stdout_synced);
            let stderr_disconnected =
                drain_sync_receiver(&self.stderr_sync_rx, token, &mut progress.stderr_synced);
            if progress.stdout_synced && progress.stderr_synced {
                if let Some(frame) = progress.control.take() {
                    self.clear_expected_token(token);
                    return WaitOutcome::Frame(frame);
                }
            }
            if let Some(status) = self.try_wait() {
                return WaitOutcome::Exited(status);
            }
            if control_disconnected
                || (stdout_disconnected && !progress.stdout_synced)
                || (stderr_disconnected && !progress.stderr_synced)
            {
                if let Some(status) = self.wait_for_exit(PROCESS_SIGNAL_GRACE) {
                    return WaitOutcome::Exited(status);
                }
                return WaitOutcome::ControlLost;
            }
            if Instant::now() >= deadline {
                return WaitOutcome::TimedOut;
            }
            thread::sleep(OUTPUT_READ_SLEEP);
        }
    }

    fn clear_expected_token(&self, token: &str) {
        let mut expected = lock_unpoison(&self.expected_token);
        if expected.as_deref() == Some(token) {
            *expected = None;
        }
    }

    fn try_wait(&self) -> Option<ExitStatus> {
        lock_unpoison(&self.child).try_wait().ok().flatten()
    }

    fn wait_for_exit(&self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn shutdown(&self) {
        if self.shutdown_started.swap(true, Ordering::SeqCst) {
            return;
        }
        lock_unpoison(&self.stdin).take();
        if self.try_wait().is_some() {
            self.terminate_remaining_group_after_exit();
            return;
        }
        let _ = signal_process_group(self.process_group_id, libc::SIGTERM);
        let deadline = Instant::now() + PROCESS_SIGNAL_GRACE;
        while Instant::now() < deadline {
            if self.try_wait().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        if self.try_wait().is_none() {
            let _ = signal_process_group(self.process_group_id, libc::SIGKILL);
            let kill_deadline = Instant::now() + PROCESS_SIGNAL_GRACE;
            while Instant::now() < kill_deadline {
                if self.try_wait().is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
        self.finish_readers();
    }

    fn terminate_remaining_group_after_exit(&self) {
        if !self.reader_threads_running() {
            self.finish_readers();
            return;
        }
        let _ = signal_process_group(self.process_group_id, libc::SIGTERM);
        let deadline = Instant::now() + PROCESS_SIGNAL_GRACE;
        while Instant::now() < deadline && self.reader_threads_running() {
            thread::sleep(Duration::from_millis(5));
        }
        // A reader still blocked after the grace period means an original
        // process-group member still owns stdout/stderr/control. The private
        // group cannot be reused while that member exists.
        if self.reader_threads_running() {
            let _ = signal_process_group(self.process_group_id, libc::SIGKILL);
        }
        self.finish_readers();
    }

    fn reader_threads_running(&self) -> bool {
        lock_unpoison(&self.reader_threads)
            .as_ref()
            .is_some_and(|handles| handles.iter().any(|handle| !handle.is_finished()))
    }

    fn finish_readers(&self) {
        self.readers_stop.store(true, Ordering::SeqCst);
        if let Some(handles) = lock_unpoison(&self.reader_threads).take() {
            for handle in handles {
                let _ = handle.join();
            }
        }
    }
}

#[cfg(unix)]
impl Drop for ShellProcess {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(unix)]
impl ShellTransport for ShellProcess {
    fn set_expected_token(&self, token: &str) {
        ShellProcess::set_expected_token(self, token);
    }

    fn write_command(&self, command: &str, token: &str) -> Result<(), ShellError> {
        ShellProcess::write_command(self, command, token)
    }

    fn wait_for_completion(
        &self,
        token: &str,
        timeout: Duration,
        progress: &mut CompletionProgress,
    ) -> WaitOutcome {
        ShellProcess::wait_for_completion(self, token, timeout, progress)
    }

    fn try_wait(&self) -> Option<ExitStatus> {
        ShellProcess::try_wait(self)
    }

    fn interrupt(&self) {
        ShellProcess::interrupt(self);
    }

    fn shutdown(&self) {
        ShellProcess::shutdown(self);
    }

    fn terminate_remaining_group_after_exit(&self) {
        ShellProcess::terminate_remaining_group_after_exit(self);
    }

    fn stdout(&self) -> &Arc<Mutex<BoundedBuffer>> {
        ShellProcess::stdout_buffer(self)
    }

    fn stderr(&self) -> &Arc<Mutex<BoundedBuffer>> {
        ShellProcess::stderr_buffer(self)
    }
}

pub const fn local_shell_supported() -> bool {
    cfg!(any(unix, windows))
}

#[cfg(any(unix, windows))]
fn ensure_local_shell_supported() -> Result<(), ShellError> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn ensure_local_shell_supported() -> Result<(), ShellError> {
    Err(persistent_shell_unsupported_error())
}

#[cfg(not(any(unix, windows)))]
fn persistent_shell_unsupported_error() -> ShellError {
    ShellError::new(
        "persistent_shell_unsupported",
        "persistent local shell is not supported on this platform",
    )
}

fn validate_launch(launch: &ShellLaunch) -> Result<(), ShellError> {
    for (field, value) in [
        ("shell_id", launch.identity.shell_id.as_str()),
        (
            "workflow_session_id",
            launch.identity.workflow_session_id.as_str(),
        ),
        (
            "runtime_project_id",
            launch.identity.runtime_project_id.as_str(),
        ),
        ("executor", launch.identity.executor.as_str()),
        ("dialect", launch.dialect.as_str()),
        ("program", launch.program.as_str()),
    ] {
        if value.trim().is_empty() || value.chars().any(char::is_control) {
            return Err(ShellError::new(
                "persistent_shell_invalid_open",
                format!("{field} must be non-empty and contain no control characters"),
            ));
        }
    }
    #[cfg(unix)]
    if !matches!(launch.dialect.as_str(), "sh" | "bash") {
        return Err(ShellError::new(
            "persistent_shell_dialect_unsupported",
            "Unix persistent shells support only sh or bash",
        ));
    }
    #[cfg(windows)]
    if launch.dialect != "powershell" {
        return Err(ShellError::new(
            "persistent_shell_dialect_unsupported",
            "Windows persistent shells require a configured PowerShell program",
        ));
    }
    if launch.args.iter().any(|arg| arg.contains('\0')) {
        return Err(ShellError::new(
            "persistent_shell_invalid_open",
            "persistent shell startup args cannot contain NUL",
        ));
    }
    #[cfg(windows)]
    if launch.args.iter().any(|arg| {
        matches!(
            arg.to_ascii_lowercase().as_str(),
            "-command" | "-encodedcommand" | "-file"
        )
    }) {
        return Err(ShellError::new(
            "persistent_shell_invalid_open",
            "Windows persistent shell startup args cannot contain PowerShell command/file payload switches",
        ));
    }
    #[cfg(unix)]
    if launch.args.iter().any(|arg| arg == "-c") {
        return Err(ShellError::new(
            "persistent_shell_invalid_open",
            "Unix persistent shell startup args cannot contain -c",
        ));
    }
    if !launch.initial_cwd.is_dir() {
        return Err(ShellError::new(
            "persistent_shell_invalid_cwd",
            "persistent shell cwd must be an existing directory",
        ));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn drain_sync_receiver(
    receiver: &Mutex<mpsc::Receiver<String>>,
    token: &str,
    synced: &mut bool,
) -> bool {
    let receiver = lock_unpoison(receiver);
    loop {
        match receiver.try_recv() {
            Ok(received) if received == token => *synced = true,
            Ok(_) => {}
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => return true,
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_COMMAND_TOKEN: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_test_command_token(token: Option<&str>) {
    TEST_COMMAND_TOKEN.with(|slot| {
        *slot.borrow_mut() = token.map(str::to_string);
    });
}

fn command_token() -> String {
    #[cfg(test)]
    if let Some(token) = TEST_COMMAND_TOKEN.with(|slot| slot.borrow().clone()) {
        return token;
    }
    Uuid::new_v4().simple().to_string()
}

/// Build the command wrapper for a remote shell that has reserved FD 7 (a dup
/// of the SSH channel's original stdout) and FD 8 (a dup of the original
/// stderr) at startup. Because an SSH exec channel has no extra control FD,
/// the control frame travels inline on the reserved stderr (FD 8) right after
/// the stderr sync marker.
///
/// Frame layout written after the user command (same magic + NUL framing as the
/// local shell, so the manager's marker parsing is shared):
///   - `WCPSO1\0{token}\0`            -> FD 7   (stdout sync boundary)
///   - `WCPSE1\0{token}\0`            -> FD 8   (stderr sync boundary)
///   - `WCPS1\0{token}\0{status}\0`   -> FD 8   (control frame: exit status)
///   - `pwd -P` output                -> FD 8   (absolute cwd)
///   - `\0`                           -> FD 8   (control frame terminator)
///
/// The remote shell's own `printf`/`pwd` builtins are used. User redirects
/// (`exec 2>&1`, etc.) cannot move the protocol targets because FD 7/8 are
/// reserved at startup, not bound to the current stdout/stderr.
pub fn remote_command_wrapper(command: &str, token: &str) -> String {
    let status_variable = format!("__wc_ps_status_{token}");
    let framed = format!(
        "\\eval {}\n\
         {status_variable}=$?\n\
         printf 'WCPSO1\\000{}\\000' >&{}\n\
         printf 'WCPSE1\\000{}\\000' >&{}\n\
         printf 'WCPS1\\000{}\\000%s\\000' \"${status_variable}\" >&{}\n\
         pwd -P >&{}\n\
         printf '\\000' >&{}\n",
        shell_quote(command),
        token,
        STDOUT_SYNC_FD,
        token,
        STDERR_SYNC_FD,
        token,
        STDERR_SYNC_FD,
        STDERR_SYNC_FD,
        STDERR_SYNC_FD,
    );
    // Same eval/alias-suppression hardening as the local command_wrapper.
    format!("\\eval {}\n", shell_quote(&framed))
}

/// Single-quote a value for a POSIX shell, escaping embedded quotes. Reused by
/// remote transports so the trusted postamble stays in one resolved builtin.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn command_wrapper(command: &str, token: &str, printf: &str, pwd: &str) -> String {
    let status_variable = format!("__wc_ps_status_{token}");
    let framed = format!(
        "\\eval {}\n\
         {status_variable}=$?\n\
         {} 'WCPSO1\\000{}\\000' >&{}\n\
         {} 'WCPSE1\\000{}\\000' >&{}\n\
         {} 'WCPS1\\000{}\\000%s\\000' \"${status_variable}\" >&{}\n\
         {} -P >&{}\n\
         {} '\\000' >&{}\n",
        shell_quote(command),
        shell_quote(printf),
        token,
        STDOUT_SYNC_FD,
        shell_quote(printf),
        token,
        STDERR_SYNC_FD,
        shell_quote(printf),
        token,
        CONTROL_FD,
        shell_quote(pwd),
        CONTROL_FD,
        shell_quote(printf),
        CONTROL_FD,
    );
    // The outer eval keeps the trusted postamble in the same already-resolved
    // builtin invocation as the user command. The inner eval gives malformed
    // command text a normal non-zero completion instead of corrupting the
    // shell parser. A leading backslash suppresses user aliases.
    format!("\\eval {}\n", shell_quote(&framed))
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn set_close_on_exec(fd: RawFd) -> std::io::Result<()> {
    // `pipe2(O_CLOEXEC)` is unavailable on Darwin. Preserve any existing
    // descriptor flags and add FD_CLOEXEC immediately after creating the pipe.
    // SAFETY: `fd` is owned by a live `File` for the duration of both calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn create_control_pipe() -> Result<(File, File), ShellError> {
    let mut fds = [-1_i32; 2];
    // Linux and Android can set close-on-exec atomically. Darwin and other Unix
    // targets use `pipe` followed immediately by `fcntl(FD_CLOEXEC)` because
    // their libc does not expose `pipe2`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: `fds` points to two valid integers and `pipe2` initializes both
    // on success.
    let pipe_result = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    // SAFETY: `fds` points to two valid integers and `pipe` initializes both on
    // success.
    let pipe_result = unsafe { libc::pipe(fds.as_mut_ptr()) };

    if pipe_result == -1 {
        return Err(ShellError::new(
            "persistent_shell_spawn_failed",
            format!(
                "failed to create persistent shell control pipe: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    // SAFETY: both descriptors were created by the successful pipe call above
    // and ownership is transferred exactly once to these `File` values.
    let reader = unsafe { File::from_raw_fd(fds[0]) };
    // SAFETY: same ownership argument as for `reader`.
    let writer = unsafe { File::from_raw_fd(fds[1]) };

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    for fd in [reader.as_raw_fd(), writer.as_raw_fd()] {
        set_close_on_exec(fd).map_err(|error| {
            ShellError::new(
                "persistent_shell_spawn_failed",
                format!("failed to secure persistent shell control pipe: {error}"),
            )
        })?;
    }

    Ok((reader, writer))
}

#[cfg(unix)]
fn spawn_shell_process(launch: &ShellLaunch) -> Result<ShellProcess, ShellError> {
    let control_printf = resolve_control_program("printf")?;
    let control_pwd = resolve_control_program("pwd")?;
    let (control_reader, control_writer) = create_control_pipe()?;
    let control_write_fd = control_writer.as_raw_fd();
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .current_dir(&launch.initial_cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .envs(&launch.env);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid`, `dup2`, and `fcntl` are async-signal-safe. The raw
        // writer fd remains open in the parent until `spawn` returns.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // Duplicate the control writer first because its inherited fd
                // can itself be 7 or 8 in the parent. Output sync fds share the
                // exact stdout/stderr pipes, providing a deterministic drain
                // boundary for each command.
                if libc::dup2(control_write_fd, CONTROL_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::dup2(libc::STDOUT_FILENO, STDOUT_SYNC_FD) == -1
                    || libc::dup2(libc::STDERR_FILENO, STDERR_SYNC_FD) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(CONTROL_FD, libc::F_SETFD, 0) == -1
                    || libc::fcntl(STDOUT_SYNC_FD, libc::F_SETFD, 0) == -1
                    || libc::fcntl(STDERR_SYNC_FD, libc::F_SETFD, 0) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let child = command.spawn().map_err(|error| {
        ShellError::new(
            "persistent_shell_spawn_failed",
            format!("failed to spawn persistent shell: {error}"),
        )
    })?;
    drop(control_writer);
    let mut child = SpawnedChildGuard::new(child);
    let process_group_id = child.process_group_id;
    let stdin = child.child_mut().stdin.take().ok_or_else(|| {
        ShellError::new(
            "persistent_shell_spawn_failed",
            "persistent shell stdin pipe was not created",
        )
    })?;
    let stdout = child.child_mut().stdout.take().ok_or_else(|| {
        ShellError::new(
            "persistent_shell_spawn_failed",
            "persistent shell stdout pipe was not created",
        )
    })?;
    let stderr = child.child_mut().stderr.take().ok_or_else(|| {
        ShellError::new(
            "persistent_shell_spawn_failed",
            "persistent shell stderr pipe was not created",
        )
    })?;

    let readers_stop = Arc::new(AtomicBool::new(false));
    let stdout_buffer = Arc::new(Mutex::new(BoundedBuffer::new(launch.max_output_bytes)));
    let stderr_buffer = Arc::new(Mutex::new(BoundedBuffer::new(launch.max_output_bytes)));
    let expected_token = Arc::new(Mutex::new(None));
    let (control_tx, control_rx) = mpsc::sync_channel(CONTROL_CHANNEL_CAPACITY);
    let (stdout_sync_tx, stdout_sync_rx) = mpsc::sync_channel(OUTPUT_SYNC_CHANNEL_CAPACITY);
    let (stderr_sync_tx, stderr_sync_rx) = mpsc::sync_channel(OUTPUT_SYNC_CHANNEL_CAPACITY);
    let handles = vec![
        spawn_output_reader(
            "stdout",
            stdout,
            Arc::clone(&stdout_buffer),
            Arc::clone(&expected_token),
            stdout_sync_tx,
            STDOUT_SYNC_MAGIC,
            Arc::clone(&readers_stop),
        )?,
        spawn_output_reader(
            "stderr",
            stderr,
            Arc::clone(&stderr_buffer),
            Arc::clone(&expected_token),
            stderr_sync_tx,
            STDERR_SYNC_MAGIC,
            Arc::clone(&readers_stop),
        )?,
        spawn_control_reader(
            control_reader,
            Arc::clone(&expected_token),
            control_tx,
            Arc::clone(&readers_stop),
        )?,
    ];
    Ok(ShellProcess {
        child: Mutex::new(child.disarm()),
        stdin: Mutex::new(Some(stdin)),
        process_group_id,
        control_printf,
        control_pwd,
        expected_token,
        control_rx: Mutex::new(control_rx),
        stdout_sync_rx: Mutex::new(stdout_sync_rx),
        stderr_sync_rx: Mutex::new(stderr_sync_rx),
        stdout: stdout_buffer,
        stderr: stderr_buffer,
        readers_stop,
        reader_threads: Mutex::new(Some(handles)),
        shutdown_started: AtomicBool::new(false),
    })
}

#[cfg(not(any(unix, windows)))]
fn spawn_shell_process(_launch: &ShellLaunch) -> Result<Box<dyn ShellTransport>, ShellError> {
    Err(persistent_shell_unsupported_error())
}

#[cfg(unix)]
fn resolve_control_program(name: &str) -> Result<String, ShellError> {
    for directory in ["/usr/bin", "/bin"] {
        let path = Path::new(directory).join(name);
        if path.is_file() {
            return Ok(path.to_string_lossy().into_owned());
        }
    }
    Err(ShellError::new(
        "persistent_shell_spawn_failed",
        format!("required persistent shell control program '{name}' was not found"),
    ))
}

#[cfg(unix)]
fn set_nonblocking(fd: RawFd) -> Result<(), ShellError> {
    // SAFETY: `fd` belongs to a live pipe owned by the caller.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(ShellError::new(
            "persistent_shell_reader_failed",
            format!(
                "failed to configure persistent shell pipe: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn spawn_output_reader(
    name: &'static str,
    mut pipe: impl Read + AsRawFd + Send + 'static,
    buffer: Arc<Mutex<BoundedBuffer>>,
    expected_token: Arc<Mutex<Option<String>>>,
    sync_sender: mpsc::SyncSender<String>,
    sync_magic: &'static [u8],
    stop: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<()>, ShellError> {
    set_nonblocking(pipe.as_raw_fd())?;
    thread::Builder::new()
        .name(format!("wc-persistent-shell-{name}"))
        .spawn(move || {
            let mut chunk = [0_u8; 8192];
            let mut pending = Vec::new();
            let mut last_synced_token: Option<String> = None;
            while !stop.load(Ordering::SeqCst) {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        pending.extend_from_slice(&chunk[..read]);
                        process_output_pending(
                            &mut pending,
                            &buffer,
                            &expected_token,
                            &sync_sender,
                            sync_magic,
                            &mut last_synced_token,
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(OUTPUT_READ_SLEEP);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            lock_unpoison(&buffer).append(&pending);
        })
        .map_err(|error| {
            ShellError::new(
                "persistent_shell_reader_failed",
                format!("failed to start persistent shell {name} reader: {error}"),
            )
        })
}

#[cfg(any(unix, windows))]
fn process_output_pending(
    pending: &mut Vec<u8>,
    buffer: &Arc<Mutex<BoundedBuffer>>,
    expected_token: &Arc<Mutex<Option<String>>>,
    sync_sender: &mpsc::SyncSender<String>,
    sync_magic: &[u8],
    last_synced_token: &mut Option<String>,
) {
    let expected = lock_unpoison(expected_token).clone();
    let Some(token) = expected else {
        lock_unpoison(buffer).append(pending);
        pending.clear();
        return;
    };
    if last_synced_token.as_deref() == Some(token.as_str()) {
        lock_unpoison(buffer).append(pending);
        pending.clear();
        return;
    }
    let marker = output_sync_marker(sync_magic, &token);
    if let Some(position) = find_bytes(pending, &marker) {
        lock_unpoison(buffer).append(&pending[..position]);
        pending.drain(..position + marker.len());
        let _ = sync_sender.send(token.clone());
        *last_synced_token = Some(token);
        lock_unpoison(buffer).append(pending);
        pending.clear();
        return;
    }
    let retained = longest_suffix_prefix(pending, &marker);
    let emit = pending.len().saturating_sub(retained);
    if emit > 0 {
        lock_unpoison(buffer).append(&pending[..emit]);
        pending.drain(..emit);
    }
}

pub fn output_sync_marker(magic: &[u8], token: &str) -> Vec<u8> {
    let mut marker = Vec::with_capacity(magic.len() + token.len() + 2);
    marker.extend_from_slice(magic);
    marker.push(0);
    marker.extend_from_slice(token.as_bytes());
    marker.push(0);
    marker
}

pub fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    (!needle.is_empty() && haystack.len() >= needle.len())
        .then(|| {
            haystack
                .windows(needle.len())
                .position(|window| window == needle)
        })
        .flatten()
}

pub fn longest_suffix_prefix(value: &[u8], marker: &[u8]) -> usize {
    let max = value.len().min(marker.len().saturating_sub(1));
    (1..=max)
        .rev()
        .find(|length| value[value.len() - length..] == marker[..*length])
        .unwrap_or(0)
}

#[cfg(unix)]
fn spawn_control_reader(
    mut pipe: File,
    expected_token: Arc<Mutex<Option<String>>>,
    sender: mpsc::SyncSender<ControlFrame>,
    stop: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<()>, ShellError> {
    set_nonblocking(pipe.as_raw_fd())?;
    thread::Builder::new()
        .name("wc-persistent-shell-control".to_string())
        .spawn(move || {
            let mut chunk = [0_u8; 1024];
            let mut field = Vec::new();
            let mut stage = 0_u8;
            let mut token = String::new();
            let mut status = 0_i32;
            while !stop.load(Ordering::SeqCst) {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        for byte in &chunk[..read] {
                            if *byte != 0 {
                                if field.len() < CONTROL_FIELD_MAX_BYTES {
                                    field.push(*byte);
                                } else {
                                    field.clear();
                                    stage = 0;
                                }
                                continue;
                            }
                            match stage {
                                0 if field == CONTROL_MAGIC => stage = 1,
                                0 => {}
                                1 => {
                                    let candidate = String::from_utf8_lossy(&field).into_owned();
                                    if lock_unpoison(&expected_token).as_deref()
                                        == Some(candidate.as_str())
                                    {
                                        token = candidate;
                                        stage = 2;
                                    } else {
                                        stage = u8::from(field == CONTROL_MAGIC);
                                    }
                                }
                                2 => match String::from_utf8_lossy(&field).parse::<i32>() {
                                    Ok(value) => {
                                        status = value;
                                        stage = 3;
                                    }
                                    Err(_) => stage = 0,
                                },
                                3 => {
                                    if field.last() == Some(&b'\n') {
                                        field.pop();
                                    }
                                    let cwd = PathBuf::from(OsString::from_vec(std::mem::take(
                                        &mut field,
                                    )));
                                    let _ = sender.try_send(ControlFrame {
                                        token: std::mem::take(&mut token),
                                        status,
                                        cwd,
                                    });
                                    stage = 0;
                                }
                                _ => stage = 0,
                            }
                            field.clear();
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(OUTPUT_READ_SLEEP);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        })
        .map_err(|error| {
            ShellError::new(
                "persistent_shell_reader_failed",
                format!("failed to start persistent shell control reader: {error}"),
            )
        })
}

#[cfg(unix)]
fn signal_process_group(process_group_id: u32, signal: i32) -> Result<(), ShellError> {
    let process_group_id = i32::try_from(process_group_id).map_err(|_| {
        ShellError::new(
            "persistent_shell_signal_failed",
            "persistent shell process group id is invalid",
        )
    })?;
    // SAFETY: the shell is placed in a private session/process group before
    // exec; negative pid targets that group only.
    if unsafe { libc::kill(-process_group_id, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(ShellError::new(
        "persistent_shell_signal_failed",
        format!("failed to signal persistent shell process group: {error}"),
    ))
}

fn spawn_idle_sweeper(weak: Weak<ManagerInner>) {
    let _ = thread::Builder::new()
        .name("wc-persistent-shell-idle".to_string())
        .spawn(move || loop {
            let Some(inner) = weak.upgrade() else {
                break;
            };
            if inner.stop_sweeper.load(Ordering::SeqCst) {
                break;
            }
            let interval = Duration::from_secs(
                (inner.idle_timeout_secs.load(Ordering::SeqCst) / 4).clamp(1, 30),
            );
            drop(inner);
            thread::sleep(interval);
            let Some(inner) = weak.upgrade() else {
                break;
            };
            if inner.stop_sweeper.load(Ordering::SeqCst) {
                break;
            }
            PersistentShellManager { inner }.sweep_idle();
        });
}

pub fn canonical_dialect(program: &str) -> Option<&'static str> {
    let basename = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    match basename.as_str() {
        "sh" => Some("sh"),
        "bash" => Some("bash"),
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => Some("powershell"),
        _ => None,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Barrier;

    /// Whether `pid` is effectively terminated: it no longer exists (`ESRCH`),
    /// or it has become a zombie (`Z` / dead `X`) in `/proc/<pid>/stat` and will
    /// never execute again.
    ///
    /// This cannot rely on `kill(pid, 0)` alone: in containers whose PID 1 does
    /// not reap orphaned children, a terminated background process lingers in
    /// the process table as a zombie and `kill(pid, 0)` keeps returning 0. A
    /// process that is still running or sleeping (states such as `R`, `S`, `D`)
    /// is reported as alive, so a shutdown that failed to terminate its
    /// descendants still fails the test.
    #[cfg(unix)]
    fn process_is_effectively_terminated(pid: i32) -> bool {
        // SAFETY: signal 0 performs an existence check without delivering a
        // signal. The pid came from the test itself.
        if unsafe { libc::kill(pid, 0) } == -1 {
            // ESRCH (or EPERM against another user's process, which cannot
            // happen here) means the process is gone.
            return true;
        }
        // The process still exists in the table. Read its state and treat a
        // zombie/dead state as terminated.
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Reaped between the kill probe and the read.
                return true;
            }
            Err(_) => {
                // Cannot determine; fall back to the kill probe (still exists).
                return false;
            }
        };
        // `/proc/<pid>/stat` is `pid (comm) state ...`; `comm` may contain
        // spaces and parentheses, so take the first token after the last `)`.
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .unwrap_or("");
        state == "Z" || state == "X"
    }

    #[cfg(unix)]
    fn launch(root: &Path, shell_id: &str, session_id: &str) -> ShellLaunch {
        ShellLaunch {
            identity: ShellIdentity {
                shell_id: shell_id.to_string(),
                workflow_session_id: session_id.to_string(),
                runtime_project_id: "agent:oe:test".to_string(),
                executor: "local".to_string(),
                client_id: None,
            },
            dialect: "bash".to_string(),
            profile: None,
            program: "bash".to_string(),
            args: vec!["--noprofile".to_string(), "--norc".to_string()],
            initial_cwd: root.to_path_buf(),
            env: std::env::vars().collect(),
            initialization: None,
            max_output_bytes: 4096,
        }
    }

    #[cfg(unix)]
    fn exec(
        manager: &PersistentShellManager,
        shell_id: &str,
        session: &str,
        command: &str,
    ) -> ShellExecResult {
        manager
            .exec(
                shell_id,
                session,
                "agent:oe:test",
                command,
                Duration::from_secs(3),
            )
            .unwrap()
    }

    #[test]
    fn control_pipe_descriptors_are_close_on_exec() {
        let (reader, writer) = create_control_pipe().unwrap();
        for fd in [reader.as_raw_fd(), writer.as_raw_fd()] {
            // SAFETY: each fd remains owned by its `File` during this query.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert_ne!(flags, -1, "failed to inspect descriptor flags");
            assert_ne!(flags & libc::FD_CLOEXEC, 0, "FD_CLOEXEC was not set");
        }
    }

    #[test]
    fn external_transport_freezes_login_cwd_from_first_control_frame() {
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        let spec = launch(
            temp.path(),
            "wc_shell_external_login",
            "wc_sess_external_login",
        );
        let process = spawn_shell_process(&spec).unwrap();
        let opened = manager
            .open_with_transport(
                spec.identity,
                spec.dialect,
                spec.profile,
                PathBuf::new(),
                spec.initialization,
                Box::new(process),
            )
            .unwrap();
        let login_cwd = temp.path().canonicalize().unwrap();
        assert_eq!(opened.cwd, login_cwd);
        assert_eq!(opened.initial_cwd, login_cwd);

        let changed = exec(
            &manager,
            "wc_shell_external_login",
            "wc_sess_external_login",
            "cd /tmp",
        );
        assert_eq!(changed.cwd, PathBuf::from("/tmp"));

        let status = manager
            .status(
                "wc_shell_external_login",
                "wc_sess_external_login",
                "agent:oe:test",
            )
            .unwrap();
        assert_eq!(status.cwd, PathBuf::from("/tmp"));
        assert_eq!(status.initial_cwd, login_cwd);

        let closed = manager
            .close(
                "wc_shell_external_login",
                "wc_sess_external_login",
                "agent:oe:test",
                "explicit_close",
            )
            .unwrap();
        assert_eq!(closed.summary.initial_cwd, login_cwd);
    }

    #[cfg(unix)]
    #[test]
    fn external_transport_replaces_symlink_seed_with_physical_cwd() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let physical = temp.path().join("physical");
        let logical = temp.path().join("logical");
        std::fs::create_dir(&physical).unwrap();
        symlink(&physical, &logical).unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        let spec = launch(
            &logical,
            "wc_shell_external_symlink",
            "wc_sess_external_symlink",
        );
        let process = spawn_shell_process(&spec).unwrap();
        let opened = manager
            .open_with_transport(
                spec.identity,
                spec.dialect,
                spec.profile,
                logical,
                spec.initialization,
                Box::new(process),
            )
            .unwrap();
        let physical = physical.canonicalize().unwrap();
        assert_eq!(opened.cwd, physical);
        assert_eq!(opened.initial_cwd, physical);

        exec(
            &manager,
            "wc_shell_external_symlink",
            "wc_sess_external_symlink",
            "cd /tmp",
        );
        let status = manager
            .status(
                "wc_shell_external_symlink",
                "wc_sess_external_symlink",
                "agent:oe:test",
            )
            .unwrap();
        assert_eq!(status.cwd, PathBuf::from("/tmp"));
        assert_eq!(status.initial_cwd, physical);
    }

    #[test]
    fn local_initial_cwd_stays_at_launch_directory_when_initialization_changes_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        let mut spec = launch(temp.path(), "wc_shell_local_init", "wc_sess_local_init");
        spec.initialization = Some("cd /tmp".to_string());
        let opened = manager.open(spec).unwrap();

        assert_eq!(opened.initial_cwd, temp.path());
        assert_eq!(opened.cwd, PathBuf::from("/tmp"));
    }

    #[test]
    fn preserves_cwd_environment_variables_functions_umask_and_shell_variables() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("nested")).unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        manager
            .open(launch(temp.path(), "wc_shell_state", "wc_sess_state"))
            .unwrap();

        assert_eq!(
            exec(&manager, "wc_shell_state", "wc_sess_state", "cd nested").exit_code,
            Some(0)
        );
        let spoofed = exec(
            &manager,
            "wc_shell_state",
            "wc_sess_state",
            "PWD=/; pwd() { printf /; }",
        );
        assert_eq!(spoofed.cwd, temp.path().join("nested"));
        exec(
            &manager,
            "wc_shell_state",
            "wc_sess_state",
            "cd .; unset -f pwd",
        );
        exec(
            &manager,
            "wc_shell_state",
            "wc_sess_state",
            "export WC_TEST_VALUE=ready; WC_LOCAL=value; wc_fn() { printf 'fn:%s' \"$WC_LOCAL\"; }; umask 027",
        );
        let observed = exec(
            &manager,
            "wc_shell_state",
            "wc_sess_state",
            "printf '%s:%s:' \"$PWD\" \"$WC_TEST_VALUE\"; wc_fn; printf ':%s' \"$(umask)\"",
        );
        assert!(observed.stdout.contains("nested:ready:fn:value:0027"));
        exec(
            &manager,
            "wc_shell_state",
            "wc_sess_state",
            "unset WC_TEST_VALUE",
        );
        let unset = exec(
            &manager,
            "wc_shell_state",
            "wc_sess_state",
            "printf '%s' \"${WC_TEST_VALUE-unset}\"",
        );
        assert_eq!(unset.stdout, "unset");
        let hardened_control = exec(
            &manager,
            "wc_shell_state",
            "wc_sess_state",
            "WC_SAVED_PATH=$PATH; enable -n printf; PATH=/definitely-missing",
        );
        assert_eq!(hardened_control.exit_code, Some(0));
        assert_eq!(hardened_control.shell_state, ShellState::Running);
        exec(
            &manager,
            "wc_shell_state",
            "wc_sess_state",
            "PATH=$WC_SAVED_PATH; enable printf; unset WC_SAVED_PATH",
        );
        exec(
            &manager,
            "wc_shell_state",
            "wc_sess_state",
            "command() { /usr/bin/printf command-fn; }; printf() { /usr/bin/printf printf-fn; }; pwd() { /usr/bin/printf pwd-fn; }",
        );
        let shadowed_builtins = exec(
            &manager,
            "wc_shell_state",
            "wc_sess_state",
            "command; printf; pwd",
        );
        assert_eq!(shadowed_builtins.stdout, "command-fnprintf-fnpwd-fn");
        assert_eq!(shadowed_builtins.shell_state, ShellState::Running);
    }

    #[test]
    fn sessions_are_isolated_and_one_shot_shell_does_not_inherit_state() {
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        manager
            .open(launch(temp.path(), "wc_shell_a", "wc_sess_a"))
            .unwrap();
        manager
            .open(launch(temp.path(), "wc_shell_b", "wc_sess_b"))
            .unwrap();
        exec(
            &manager,
            "wc_shell_a",
            "wc_sess_a",
            "export WC_ISOLATED=only_a",
        );
        let other = exec(
            &manager,
            "wc_shell_b",
            "wc_sess_b",
            "printf '%s' \"${WC_ISOLATED-unset}\"",
        );
        assert_eq!(other.stdout, "unset");
        let one_shot = Command::new("sh")
            .arg("-c")
            .arg("printf '%s' \"${WC_ISOLATED-unset}\"")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&one_shot.stdout), "unset");
        assert_eq!(
            manager.close_project("agent:oe:test", "project_disabled"),
            2
        );
        assert_eq!(manager.active_count(), 0);
    }

    #[test]
    fn only_one_active_shell_per_session_and_old_id_is_stale_after_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        manager
            .open(launch(temp.path(), "wc_shell_old", "wc_sess_one"))
            .unwrap();
        let mismatch = manager
            .status("wc_shell_old", "wc_sess_other", "agent:oe:other-project")
            .unwrap_err();
        assert_eq!(mismatch.code, "persistent_shell_not_found");
        let duplicate = manager
            .open(launch(temp.path(), "wc_shell_duplicate", "wc_sess_one"))
            .unwrap_err();
        assert_eq!(duplicate.code, "persistent_shell_already_open");
        manager
            .close(
                "wc_shell_old",
                "wc_sess_one",
                "agent:oe:test",
                "explicit_close",
            )
            .unwrap();
        manager
            .open(launch(temp.path(), "wc_shell_new", "wc_sess_one"))
            .unwrap();
        let stale = manager
            .exec(
                "wc_shell_old",
                "wc_sess_one",
                "agent:oe:test",
                "true",
                Duration::from_secs(1),
            )
            .unwrap_err();
        assert_eq!(stale.code, "persistent_shell_stale");
    }

    #[test]
    fn global_shell_limit_is_enforced() {
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits {
            max_shells: 1,
            ..ShellLimits::default()
        });
        manager
            .open(launch(temp.path(), "wc_shell_limit_a", "wc_sess_limit_a"))
            .unwrap();
        let limited = manager
            .open(launch(temp.path(), "wc_shell_limit_b", "wc_sess_limit_b"))
            .unwrap_err();
        assert_eq!(limited.code, "persistent_shell_limit_reached");
        assert_eq!(manager.active_count(), 1);
    }

    #[test]
    fn close_is_idempotent_and_exit_is_observable() {
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        manager
            .open(launch(temp.path(), "wc_shell_close", "wc_sess_close"))
            .unwrap();
        let first = manager
            .close(
                "wc_shell_close",
                "wc_sess_close",
                "agent:oe:test",
                "explicit_close",
            )
            .unwrap();
        assert!(!first.already_closed);
        let second = manager
            .close(
                "wc_shell_close",
                "wc_sess_close",
                "agent:oe:test",
                "explicit_close",
            )
            .unwrap();
        assert!(second.already_closed);

        manager
            .open(launch(temp.path(), "wc_shell_exit", "wc_sess_exit"))
            .unwrap();
        let exited = exec(&manager, "wc_shell_exit", "wc_sess_exit", "exit 7");
        assert_eq!(exited.shell_state, ShellState::Exited);
        assert!(exited.command_completed);
        assert_eq!(exited.exit_code, Some(7));

        manager
            .open(launch(
                temp.path(),
                "wc_shell_exit_background",
                "wc_sess_exit_background",
            ))
            .unwrap();
        let background_exit = exec(
            &manager,
            "wc_shell_exit_background",
            "wc_sess_exit_background",
            "sleep 30 & echo $! > background.pid; exit 0",
        );
        assert_eq!(background_exit.shell_state, ShellState::Exited);
        let background_pid: i32 = std::fs::read_to_string(temp.path().join("background.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if process_is_effectively_terminated(background_pid) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        // The background child must be terminated: gone from the process table
        // (ESRCH) or reduced to a zombie that will never run again. The
        // container's PID 1 does not reap orphans, so a lingering zombie is
        // still a successful shutdown.
        assert!(
            process_is_effectively_terminated(background_pid),
            "background child {background_pid} must be terminated"
        );
    }

    #[test]
    fn initialization_output_is_not_attributed_to_the_first_command() {
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        let mut spec = launch(temp.path(), "wc_shell_init", "wc_sess_init");
        spec.initialization = Some("printf initialization-output".to_string());
        manager.open(spec).unwrap();

        let first = exec(
            &manager,
            "wc_shell_init",
            "wc_sess_init",
            "printf user-output",
        );
        assert_eq!(first.stdout, "user-output");
    }

    #[test]
    fn concurrent_exec_returns_busy_without_mixing_output() {
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        manager
            .open(launch(temp.path(), "wc_shell_busy", "wc_sess_busy"))
            .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let worker_manager = manager.clone();
        let worker_barrier = Arc::clone(&barrier);
        let worker = thread::spawn(move || {
            worker_barrier.wait();
            worker_manager
                .exec(
                    "wc_shell_busy",
                    "wc_sess_busy",
                    "agent:oe:test",
                    "sleep 0.2; printf first",
                    Duration::from_secs(2),
                )
                .unwrap()
        });
        barrier.wait();
        while !manager
            .status("wc_shell_busy", "wc_sess_busy", "agent:oe:test")
            .unwrap()
            .busy
        {
            thread::yield_now();
        }
        let busy = manager
            .exec(
                "wc_shell_busy",
                "wc_sess_busy",
                "agent:oe:test",
                "printf second",
                Duration::from_secs(1),
            )
            .unwrap_err();
        assert_eq!(busy.code, "shell_busy");
        assert_eq!(worker.join().unwrap().stdout, "first");
    }

    #[test]
    fn close_during_exec_never_resurrects_the_shell() {
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        manager
            .open(launch(
                temp.path(),
                "wc_shell_close_busy",
                "wc_sess_close_busy",
            ))
            .unwrap();
        let worker_manager = manager.clone();
        let worker = thread::spawn(move || {
            worker_manager.exec(
                "wc_shell_close_busy",
                "wc_sess_close_busy",
                "agent:oe:test",
                "sleep 5",
                Duration::from_secs(10),
            )
        });
        while !manager
            .status("wc_shell_close_busy", "wc_sess_close_busy", "agent:oe:test")
            .unwrap()
            .busy
        {
            thread::yield_now();
        }

        manager
            .close(
                "wc_shell_close_busy",
                "wc_sess_close_busy",
                "agent:oe:test",
                "workflow_session_closed",
            )
            .unwrap();
        if let Ok(result) = worker.join().unwrap() {
            assert_ne!(result.shell_state, ShellState::Running);
        }
        let closed = manager
            .status("wc_shell_close_busy", "wc_sess_close_busy", "agent:oe:test")
            .unwrap();
        assert_eq!(closed.state, ShellState::Closed);
        assert_eq!(manager.active_count(), 0);
        manager
            .open(launch(
                temp.path(),
                "wc_shell_after_close_busy",
                "wc_sess_close_busy",
            ))
            .unwrap();
    }

    #[test]
    fn marker_like_and_large_output_are_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        let mut spec = launch(temp.path(), "wc_shell_output", "wc_sess_output");
        spec.max_output_bytes = 1024;
        manager.open(spec).unwrap();
        let result = exec(
            &manager,
            "wc_shell_output",
            "wc_sess_output",
            "(printf 'WCPS1 fake background marker\\n') & sleep 0.15; i=0; while [ \"$i\" -lt 5000 ]; do printf x; i=$((i+1)); done",
        );
        assert!(result.command_completed);
        assert!(result.duration_ms >= 100);
        assert!(result.stdout_truncated);
        assert!(result.stdout.len() <= 1024);
    }

    #[test]
    fn output_sync_parser_strips_split_markers() {
        let buffer = Arc::new(Mutex::new(BoundedBuffer::new(4096)));
        let expected = Arc::new(Mutex::new(Some("token123".to_string())));
        let (sender, receiver) = mpsc::sync_channel(1);
        let marker = output_sync_marker(STDOUT_SYNC_MAGIC, "token123");
        let split = marker.len() / 2;
        let mut pending = b"before".to_vec();
        pending.extend_from_slice(&marker[..split]);
        let mut last = None;
        process_output_pending(
            &mut pending,
            &buffer,
            &expected,
            &sender,
            STDOUT_SYNC_MAGIC,
            &mut last,
        );
        assert!(receiver.try_recv().is_err());
        pending.extend_from_slice(&marker[split..]);
        pending.extend_from_slice(b"after");
        process_output_pending(
            &mut pending,
            &buffer,
            &expected,
            &sender,
            STDOUT_SYNC_MAGIC,
            &mut last,
        );
        assert_eq!(receiver.try_recv().unwrap(), "token123");
        assert_eq!(lock_unpoison(&buffer).snapshot_since(0).0, "beforeafter");
    }

    #[test]
    fn stdout_and_stderr_boundaries_are_synchronized_without_marker_leaks() {
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        let mut spec = launch(temp.path(), "wc_shell_stream_sync", "wc_sess_stream_sync");
        spec.max_output_bytes = 64 * 1024;
        manager.open(spec).unwrap();
        let first = exec(
            &manager,
            "wc_shell_stream_sync",
            "wc_sess_stream_sync",
            "i=0; while [ \"$i\" -lt 20000 ]; do printf o; printf e >&2; i=$((i+1)); done",
        );
        assert_eq!(first.stdout.len(), 20000);
        assert_eq!(first.stderr.len(), 20000);
        assert!(!first.stdout.contains("WCPSO1"));
        assert!(!first.stderr.contains("WCPSE1"));
        let next = exec(
            &manager,
            "wc_shell_stream_sync",
            "wc_sess_stream_sync",
            "printf clean; printf error >&2",
        );
        assert_eq!(next.stdout, "clean");
        assert_eq!(next.stderr, "error");
    }

    #[test]
    fn timeout_recovers_or_requires_reset_but_never_accepts_unsynchronized_work() {
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        manager
            .open(launch(temp.path(), "wc_shell_timeout", "wc_sess_timeout"))
            .unwrap();
        let timed = manager
            .exec(
                "wc_shell_timeout",
                "wc_sess_timeout",
                "agent:oe:test",
                "sleep 5",
                Duration::from_millis(100),
            )
            .unwrap();
        assert_eq!(timed.execution_state, "timed_out");
        if timed.shell_state == ShellState::Running {
            let next = exec(
                &manager,
                "wc_shell_timeout",
                "wc_sess_timeout",
                "printf synchronized",
            );
            assert_eq!(next.stdout, "synchronized");
        } else {
            assert_eq!(timed.error_code.as_deref(), Some("shell_reset_required"));
            assert!(manager
                .exec(
                    "wc_shell_timeout",
                    "wc_sess_timeout",
                    "agent:oe:test",
                    "printf forbidden",
                    Duration::from_secs(1),
                )
                .is_err());
        }
    }

    #[test]
    fn idle_timeout_reclaims_only_idle_shells() {
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits {
            idle_timeout: Duration::from_secs(1),
            ..ShellLimits::default()
        });
        manager
            .open(launch(temp.path(), "wc_shell_idle", "wc_sess_idle"))
            .unwrap();
        thread::sleep(Duration::from_millis(1100));
        manager.sweep_idle();
        let status = manager
            .status("wc_shell_idle", "wc_sess_idle", "agent:oe:test")
            .unwrap();
        assert_eq!(status.state, ShellState::Closed);
        assert_eq!(status.close_reason.as_deref(), Some("idle_timeout"));
        assert_eq!(manager.active_count(), 0);

        manager
            .open(launch(
                temp.path(),
                "wc_shell_idle_busy",
                "wc_sess_idle_busy",
            ))
            .unwrap();
        let worker_manager = manager.clone();
        let worker = thread::spawn(move || {
            worker_manager
                .exec(
                    "wc_shell_idle_busy",
                    "wc_sess_idle_busy",
                    "agent:oe:test",
                    "sleep 1.3",
                    Duration::from_secs(2),
                )
                .unwrap()
        });
        while !manager
            .status("wc_shell_idle_busy", "wc_sess_idle_busy", "agent:oe:test")
            .unwrap()
            .busy
        {
            thread::yield_now();
        }
        thread::sleep(Duration::from_millis(1100));
        assert_eq!(manager.sweep_idle(), 0);
        assert!(manager
            .status("wc_shell_idle_busy", "wc_sess_idle_busy", "agent:oe:test",)
            .unwrap()
            .state
            .is_active());
        assert!(worker.join().unwrap().command_completed);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::sync::{Barrier, Mutex};

    const PROJECT: &str = "agent:msi:test";
    // These fixtures launch real Windows PowerShell/PowerShell 7 processes. On
    // small hosted runners, starting all eleven cold shells at once can exceed
    // the production initialization deadline even though each shell is healthy.
    // Serialize only this real-process test module; production concurrency and
    // the rest of the Rust test suite remain unchanged.
    static WINDOWS_PROCESS_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn windows_process_test_guard() -> std::sync::MutexGuard<'static, ()> {
        WINDOWS_PROCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn launch(root: &Path, shell_id: &str, session_id: &str) -> ShellLaunch {
        launch_with_program(root, shell_id, session_id, "powershell.exe")
    }

    fn launch_with_program(
        root: &Path,
        shell_id: &str,
        session_id: &str,
        program: &str,
    ) -> ShellLaunch {
        let env = std::env::vars().collect::<HashMap<_, _>>();
        ShellLaunch {
            identity: ShellIdentity {
                shell_id: shell_id.to_string(),
                workflow_session_id: session_id.to_string(),
                runtime_project_id: PROJECT.to_string(),
                executor: "agent".to_string(),
                client_id: Some("msi".to_string()),
            },
            dialect: "powershell".to_string(),
            profile: None,
            program: program.to_string(),
            args: vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
            ],
            initial_cwd: root.to_path_buf(),
            env,
            initialization: None,
            max_output_bytes: 64 * 1024,
        }
    }

    fn exec(
        manager: &PersistentShellManager,
        shell_id: &str,
        session_id: &str,
        command: &str,
    ) -> ShellExecResult {
        manager
            .exec(
                shell_id,
                session_id,
                PROJECT,
                command,
                Duration::from_secs(5),
            )
            .unwrap()
    }

    fn process_alive(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0u32;
        let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
        unsafe { CloseHandle(handle) };
        ok == 1 && exit_code == 259
    }

    #[test]
    fn state_stdout_stderr_and_unicode_persist() {
        let _guard = windows_process_test_guard();
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("sub")).unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        manager
            .open(launch(temp.path(), "wc_shell_state", "wc_sess_state"))
            .unwrap();

        let set = exec(
            &manager,
            "wc_shell_state",
            "wc_sess_state",
            "$env:WEBCODEX_PERSIST_TEST='alpha'; Set-Location -LiteralPath 'sub'; $WC_LOCAL='beta'; function WC_FN { [Console]::Out.Write('fn') }",
        );
        assert_eq!(set.exit_code, Some(0));
        let observed = exec(
            &manager,
            "wc_shell_state",
            "wc_sess_state",
            "[Console]::Out.Write($env:WEBCODEX_PERSIST_TEST + '|' + (Get-Location).Path + '|' + $WC_LOCAL + '|'); WC_FN; [Console]::Error.Write('stderr-only')",
        );
        assert!(observed.stdout.starts_with("alpha|"), "{}", observed.stdout);
        assert!(
            observed.stdout.contains("\\sub|beta|fn"),
            "{}",
            observed.stdout
        );
        assert_eq!(observed.stderr, "stderr-only");

        let unicode = exec(
            &manager,
            "wc_shell_state",
            "wc_sess_state",
            "[Console]::Out.Write(\"hello`r`n中文`r`n🙂`r`nmixed ASCII + Unicode\")",
        );
        assert_eq!(
            unicode.stdout,
            "hello\r\n中文\r\n🙂\r\nmixed ASCII + Unicode"
        );
        assert!(!unicode.stdout.contains("WCPSO1"));
        assert!(!unicode.stderr.contains("WCPSE1"));
        let host_unicode = exec(
            &manager,
            "wc_shell_state",
            "wc_sess_state",
            "Write-Output '中文🙂 host-output'",
        );
        assert!(
            host_unicode.stdout.contains("中文🙂 host-output"),
            "{:?}",
            host_unicode.stdout
        );
    }

    #[test]
    fn command_failure_does_not_lose_shell() {
        let _guard = windows_process_test_guard();
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        manager
            .open(launch(temp.path(), "wc_shell_failure", "wc_sess_failure"))
            .unwrap();
        let failed = exec(
            &manager,
            "wc_shell_failure",
            "wc_sess_failure",
            "Write-Error 'expected-failure'",
        );
        assert_eq!(failed.exit_code, Some(1));
        assert_eq!(failed.shell_state, ShellState::Running);
        assert!(
            failed.stderr.contains("expected-failure"),
            "stdout={:?} stderr={:?}",
            failed.stdout,
            failed.stderr
        );
        let recovered = exec(
            &manager,
            "wc_shell_failure",
            "wc_sess_failure",
            "Write-Error 'transient-error'; [Console]::Out.Write('recovered')",
        );
        assert_eq!(recovered.exit_code, Some(0));
        assert_eq!(recovered.stdout, "recovered");
        assert!(recovered.stderr.contains("transient-error"));

        let native_failed = exec(
            &manager,
            "wc_shell_failure",
            "wc_sess_failure",
            "cmd.exe /d /c exit 5",
        );
        assert_eq!(native_failed.exit_code, Some(5));
        let native_recovered = exec(
            &manager,
            "wc_shell_failure",
            "wc_sess_failure",
            "cmd.exe /d /c exit 5; [Console]::Out.Write('after-native')",
        );
        assert_eq!(native_recovered.exit_code, Some(0));
        assert_eq!(native_recovered.stdout, "after-native");

        let next = exec(
            &manager,
            "wc_shell_failure",
            "wc_sess_failure",
            "[Console]::Out.Write('still-running')",
        );
        assert_eq!(next.stdout, "still-running");
    }

    fn assert_status_integrity(program: &str, label: &str) {
        let _guard = windows_process_test_guard();
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("sub")).unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        let shell_id = format!("wc_shell_status_{label}");
        let session_id = format!("wc_sess_status_{label}");
        manager
            .open(launch_with_program(
                temp.path(),
                &shell_id,
                &session_id,
                program,
            ))
            .unwrap();

        let run = |command: &str| exec(&manager, &shell_id, &session_id, command);

        let early_return = run("return");
        assert_eq!(early_return.exit_code, Some(0), "{label}: plain return");
        assert_eq!(early_return.shell_state, ShellState::Running);

        let shadow_renderer =
            run("function global:Out-Default { process { } end { $global:LASTEXITCODE = 0 } }");
        assert_eq!(
            shadow_renderer.exit_code,
            Some(0),
            "{label}: renderer-shadow setup"
        );
        let shadowed_native_return = run("cmd.exe /d /c exit 7; return");
        assert_eq!(
            shadowed_native_return.exit_code,
            Some(7),
            "{label}: user-defined Out-Default intercepted trusted status capture"
        );

        let native_failed = run("cmd.exe /d /c exit 7");
        assert_eq!(native_failed.exit_code, Some(7), "{label}: native failure");

        let native_return = run("cmd.exe /d /c exit 7; return");
        assert_eq!(
            native_return.exit_code,
            Some(7),
            "{label}: top-level return skipped trusted native status"
        );

        let forged_success = run(r#"cmd.exe /d /c exit 7
Microsoft.PowerShell.Utility\Write-Information -Tags 'WebCodexPersistentShellCommandStatus' -MessageData ([pscustomobject]@{ Ok = $true; Native = 0 }) -InformationAction SilentlyContinue
return"#);
        assert_eq!(
            forged_success.exit_code,
            Some(7),
            "{label}: forged success status overrode native failure"
        );

        let forged_powershell_success = run(r#"Write-Error 'forged-powershell-failure'
Microsoft.PowerShell.Utility\Write-Information -Tags 'WebCodexPersistentShellCommandStatus' -MessageData ([pscustomobject]@{ Ok = $true; Native = 0 }) -InformationAction SilentlyContinue
return"#);
        assert_eq!(
            forged_powershell_success.exit_code,
            Some(1),
            "{label}: forged success status overrode PowerShell failure"
        );

        let forged_without_return = run(r#"cmd.exe /d /c exit 7
Microsoft.PowerShell.Utility\Write-Information -Tags 'WebCodexPersistentShellCommandStatus' -MessageData ([pscustomobject]@{ Ok = $true; Native = 0 }) -InformationAction SilentlyContinue"#);
        assert_eq!(
            forged_without_return.exit_code,
            Some(7),
            "{label}: forged Information status became command authority"
        );

        let forged_failure = run(r#"Write-Output 'real-success'
Microsoft.PowerShell.Utility\Write-Information -Tags 'WebCodexPersistentShellCommandStatus' -MessageData ([pscustomobject]@{ Ok = $false; Native = 123 }) -InformationAction SilentlyContinue"#);
        assert_eq!(
            forged_failure.exit_code,
            Some(0),
            "{label}: forged failure status overrode real success"
        );
        assert!(forged_failure.stdout.contains("real-success"));

        let non_terminating = run("Write-Error 'status-non-terminating'");
        assert_eq!(
            non_terminating.exit_code,
            Some(1),
            "{label}: non-terminating error"
        );
        let recovered = run("Write-Error 'status-recovered'; Write-Output 'recovered'");
        assert_eq!(
            recovered.exit_code,
            Some(0),
            "{label}: historical non-terminating error changed final success semantics"
        );
        assert!(recovered.stdout.contains("recovered"));

        let terminating = run("throw 'status-terminating'");
        assert_eq!(terminating.exit_code, Some(1), "{label}: terminating error");
        assert_eq!(terminating.shell_state, ShellState::Running);

        let parse_failure = run("if (");
        assert_eq!(parse_failure.exit_code, Some(1), "{label}: parse failure");
        assert_eq!(parse_failure.shell_state, ShellState::Running);

        assert_eq!(
            run("cmd.exe /d /c exit 0").exit_code,
            Some(0),
            "{label}: successful native command"
        );
        assert_eq!(
            run("Write-Output 'ordinary-success'").exit_code,
            Some(0),
            "{label}: ordinary PowerShell success"
        );

        let state_set = run(
            "$WC_STATUS_LOCAL='alpha'; function WC_STATUS_FN { [Console]::Out.Write('fn') }; Set-Location -LiteralPath 'sub'",
        );
        assert_eq!(state_set.exit_code, Some(0), "{label}: state setup");
        let state_observed = run(
            "[Console]::Out.Write($WC_STATUS_LOCAL + '|' + (Get-Location).Path + '|'); WC_STATUS_FN",
        );
        assert!(
            state_observed.stdout.contains("alpha|") && state_observed.stdout.contains("\\sub|fn"),
            "{label}: persistent variable/function/cwd state regressed: {:?}",
            state_observed.stdout
        );

        let next = run("Write-Output 'after-adversarial'");
        assert_eq!(next.exit_code, Some(0), "{label}: shell did not recover");
        assert!(next.stdout.contains("after-adversarial"));
        assert_eq!(next.shell_state, ShellState::Running);
    }

    #[test]
    fn user_command_status_is_host_authoritative() {
        assert_status_integrity("powershell.exe", "windows_powershell");
    }

    #[test]
    fn configured_pwsh_user_command_status_is_host_authoritative() {
        let Ok(program) = std::env::var("WEBCODEX_TEST_PWSH") else {
            return;
        };
        assert!(
            Path::new(&program).is_file(),
            "configured pwsh does not exist: {program}"
        );
        assert_status_integrity(&program, "powershell_7");
    }

    #[test]
    fn shell_exit_is_terminal_and_next_exec_fails_closed() {
        let _guard = windows_process_test_guard();
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        manager
            .open(launch(temp.path(), "wc_shell_exit", "wc_sess_exit"))
            .unwrap();
        let exited = exec(&manager, "wc_shell_exit", "wc_sess_exit", "exit 7");
        assert_eq!(exited.shell_state, ShellState::Exited);
        assert_eq!(exited.exit_code, Some(7));
        let error = manager
            .exec(
                "wc_shell_exit",
                "wc_sess_exit",
                PROJECT,
                "[Console]::Out.Write('forbidden')",
                Duration::from_secs(1),
            )
            .unwrap_err();
        assert_eq!(error.code, "persistent_shell_stale");
    }

    #[test]
    fn timeout_poisoning_is_bounded_and_never_reuses_uncertain_stream() {
        let _guard = windows_process_test_guard();
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        manager
            .open(launch(temp.path(), "wc_shell_timeout", "wc_sess_timeout"))
            .unwrap();
        let started = Instant::now();
        let timed = manager
            .exec(
                "wc_shell_timeout",
                "wc_sess_timeout",
                PROJECT,
                "Start-Sleep -Seconds 5",
                Duration::from_millis(100),
            )
            .unwrap();
        assert_eq!(timed.execution_state, "timed_out");
        assert_eq!(timed.shell_state, ShellState::Poisoned);
        assert_eq!(timed.error_code.as_deref(), Some("shell_reset_required"));
        assert!(started.elapsed() < Duration::from_secs(3));
        let error = manager
            .exec(
                "wc_shell_timeout",
                "wc_sess_timeout",
                PROJECT,
                "[Console]::Out.Write('forbidden')",
                Duration::from_secs(1),
            )
            .unwrap_err();
        assert_eq!(error.code, "persistent_shell_stale");
    }

    #[test]
    fn marker_like_user_output_cannot_complete_control_framing() {
        let _guard = windows_process_test_guard();
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        manager
            .open(launch(temp.path(), "wc_shell_marker", "wc_sess_marker"))
            .unwrap();
        let result = exec(
            &manager,
            "wc_shell_marker",
            "wc_sess_marker",
            "[Console]::Out.Write('WCPS1 fake WCPSO1 fake WCPSE1 fake'); Start-Sleep -Milliseconds 150; [Console]::Out.Write('|done')",
        );
        assert!(result.command_completed);
        assert!(result.duration_ms >= 100);
        assert_eq!(result.stdout, "WCPS1 fake WCPSO1 fake WCPSE1 fake|done");
    }

    const FORGE_PRIVATE_COMPLETION_COMMAND: &str = r#"
$argv = [Environment]::GetCommandLineArgs()
[Console]::Out.WriteLine('ARGV|' + ($argv -join '|'))
$variables = @(Get-Variable)
$tokenCandidates = @()
$controlPathCandidates = @()
$visible = @()
foreach ($variable in $variables) {
    $value = ''
    try { $value = [string]$variable.Value } catch { $value = '<unprintable>' }
    $visible += ('VAR|' + $variable.Name + '|' + $value)
    if ($value -match '^[0-9a-f]{32}$') { $tokenCandidates += $value }
    if ($value -match '(?i)\.frame$') { $controlPathCandidates += $value }
}
# Inspect the live function provider directly. `Get-Command -CommandType Function`
# can trigger module discovery on Windows PowerShell 5.1 and has taken tens of
# seconds on hosted CI; module discovery is unrelated to whether this command
# can observe private framing state through functions already in its session.
$visible += @(Get-ChildItem -LiteralPath 'Function:\' | ForEach-Object { 'FN|' + $_.Name })
$visible += @([System.Management.Automation.Runspaces.Runspace]::GetRunspaces() | ForEach-Object { 'RUNSPACE|' + $_.InstanceId + '|' + $_.RunspaceAvailability })
$visible += @([WebCodexPersistentShell.Controller].GetFields([Reflection.BindingFlags]'Public,NonPublic,Static') | ForEach-Object { 'CONTROLLER_FIELD|' + $_.Name + '|' + [string]$_.GetValue($null) })
$visible += @($Host.GetType().GetFields([Reflection.BindingFlags]'Public,NonPublic,Instance') | ForEach-Object { try { 'HOST_FIELD|' + $_.Name + '|' + [string]$_.GetValue($Host) } catch { 'HOST_FIELD|' + $_.Name + '|<unreadable>' } })
$bootstrap = @($argv | Where-Object { $_ -match '(?i)bootstrap\.ps1$' } | Select-Object -First 1)
if ($bootstrap.Count -eq 1 -and (Test-Path -LiteralPath $bootstrap[0])) {
    $visible += ('BOOTSTRAP_PATH|' + $bootstrap[0])
    $visible += ('BOOTSTRAP_TEXT|' + (Get-Content -LiteralPath $bootstrap[0] -Raw))
    $visible += @(Get-ChildItem -LiteralPath (Split-Path -Parent $bootstrap[0]) -Force | ForEach-Object { 'CONTROL_DIR_ENTRY|' + $_.FullName })
}
foreach ($candidateToken in @($tokenCandidates | Select-Object -Unique)) {
    foreach ($candidatePath in @($controlPathCandidates | Select-Object -Unique)) {
        try {
            $candidateCwd = (Get-Location).Path
            $candidateControl = 'WCPS1' + [char]0 + $candidateToken + [char]0 + '0' + [char]0 + $candidateCwd + [char]0
            [IO.File]::WriteAllBytes($candidatePath + '.tmp', [Text.Encoding]::UTF8.GetBytes($candidateControl))
            if (Test-Path -LiteralPath $candidatePath) { Remove-Item -LiteralPath $candidatePath -Force }
            [IO.File]::Move($candidatePath + '.tmp', $candidatePath)
        } catch { }
        [Console]::Out.Write('WCPSO1' + [char]0 + $candidateToken + [char]0)
        [Console]::Error.Write('WCPSE1' + [char]0 + $candidateToken + [char]0)
    }
}
[Console]::Out.WriteLine('VISIBLE_BEGIN')
[Console]::Out.WriteLine(($visible -join "`n"))
[Console]::Out.WriteLine('VISIBLE_END')
[Console]::Out.Write('WCPS1 fake WCPSO1 fake WCPSE1 fake|before-sleep')
Start-Sleep -Milliseconds 500
[Console]::Out.Write('|after-sleep')
"#;

    fn assert_private_completion_isolation(program: &str, label: &str, token: &'static str) {
        let _guard = windows_process_test_guard();
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        let shell_id = format!("wc_shell_forge_{label}");
        let session_id = format!("wc_sess_forge_{label}");
        let mut launch = launch_with_program(temp.path(), &shell_id, &session_id, program);
        // This adversarial regression intentionally inventories PowerShell-visible
        // state. GitHub's Windows image can expose substantially more host/module
        // metadata than a local runner, so retain a larger but still bounded test
        // transcript rather than silently dropping the earliest evidence.
        launch.max_output_bytes = 1024 * 1024;
        manager.open(launch).unwrap();

        let worker_manager = manager.clone();
        let worker_shell_id = shell_id.clone();
        let worker_session_id = session_id.clone();
        let worker = thread::spawn(move || {
            set_test_command_token(Some(token));
            let result = worker_manager.exec(
                &worker_shell_id,
                &worker_session_id,
                PROJECT,
                FORGE_PRIVATE_COMPLETION_COMMAND,
                // Windows CI can spend several seconds in PowerShell/.NET
                // introspection before reaching the deliberate 500ms sleep.
                // Keep the security assertions time-relative below, but give
                // the command enough bounded wall-clock budget to finish on a
                // cold or contended runner.
                Duration::from_secs(30),
            );
            set_test_command_token(None);
            result.unwrap()
        });

        let busy_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if manager
                .status(&shell_id, &session_id, PROJECT)
                .unwrap()
                .busy
            {
                break;
            }
            assert!(
                !worker.is_finished(),
                "{label}: adversarial command completed before busy state was observable"
            );
            assert!(
                Instant::now() < busy_deadline,
                "{label}: adversarial command never entered busy state"
            );
            thread::sleep(Duration::from_millis(5));
        }

        // The reviewed bug released this guard while the user command was still
        // sleeping after forging the transport's visible token/control frame.
        thread::sleep(Duration::from_millis(150));
        let busy = manager
            .exec(
                &shell_id,
                &session_id,
                PROJECT,
                "[Console]::Out.Write('must-not-run')",
                Duration::from_secs(1),
            )
            .unwrap_err();
        assert_eq!(
            busy.code, "shell_busy",
            "{label}: busy guard released early"
        );

        let result = worker.join().unwrap();
        assert!(
            result.command_completed,
            "{label}: command did not complete: execution_state={} shell_state={:?} error_code={:?}",
            result.execution_state,
            result.shell_state,
            result.error_code
        );
        assert_eq!(result.shell_state, ShellState::Running);
        assert!(
            !result.stdout_truncated && !result.stderr_truncated,
            "{label}: adversarial visibility evidence was truncated"
        );
        assert!(
            result.duration_ms >= 400,
            "{label}: completion arrived before the 500ms user sleep returned: {}ms",
            result.duration_ms
        );
        assert!(
            result.stdout.contains("|after-sleep"),
            "{label}: trailing user output was not attributed to the command: {:?}",
            result.stdout
        );
        assert!(result.stdout.contains("WCPS1 fake WCPSO1 fake WCPSE1 fake"));
        assert!(
            !result.stdout.contains(token) && !result.stderr.contains(token),
            "{label}: active correlation token leaked into user-visible state"
        );

        let argv_line = result
            .stdout
            .lines()
            .find(|line| line.starts_with("ARGV|"))
            .unwrap_or_else(|| panic!("{label}: process argv was not observed"));
        let bootstrap = argv_line
            .split('|')
            .skip(1)
            .find(|value| value.to_ascii_lowercase().ends_with("bootstrap.ps1"))
            .unwrap_or_else(|| panic!("{label}: bootstrap path was not discoverable from argv"));
        let expected_control_path = Path::new(bootstrap)
            .parent()
            .unwrap()
            .join(format!("{token}.frame"))
            .to_string_lossy()
            .to_string();
        let visible_output = format!("{}\n{}", result.stdout, result.stderr).to_ascii_lowercase();
        assert!(
            !visible_output.contains(&expected_control_path.to_ascii_lowercase()),
            "{label}: active control publication target leaked into user-visible state"
        );
        assert!(
            result.stdout.contains("BOOTSTRAP_TEXT|"),
            "{label}: test did not inspect the ordinary bootstrap file"
        );

        let clean = exec(
            &manager,
            &shell_id,
            &session_id,
            "[Console]::Out.Write('clean')",
        );
        assert_eq!(
            clean.stdout, "clean",
            "{label}: prior stdout leaked forward"
        );
        assert!(
            clean.stderr.is_empty(),
            "{label}: prior stderr leaked forward"
        );
    }

    #[test]
    fn user_command_cannot_forge_private_completion() {
        assert_private_completion_isolation(
            "powershell.exe",
            "windows_powershell",
            "13579bdf2468ace013579bdf2468ace0",
        );
    }

    #[test]
    fn configured_pwsh_user_command_cannot_forge_private_completion() {
        let Ok(program) = std::env::var("WEBCODEX_TEST_PWSH") else {
            return;
        };
        assert!(
            Path::new(&program).is_file(),
            "configured pwsh does not exist: {program}"
        );
        assert_private_completion_isolation(
            &program,
            "powershell_7",
            "02468ace13579bdf02468ace13579bdf",
        );
    }

    #[test]
    fn concurrent_exec_is_serialized_by_busy_guard() {
        let _guard = windows_process_test_guard();
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        manager
            .open(launch(temp.path(), "wc_shell_busy_win", "wc_sess_busy_win"))
            .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let worker_manager = manager.clone();
        let worker_barrier = Arc::clone(&barrier);
        let worker = thread::spawn(move || {
            worker_barrier.wait();
            worker_manager
                .exec(
                    "wc_shell_busy_win",
                    "wc_sess_busy_win",
                    PROJECT,
                    "Start-Sleep -Milliseconds 300; [Console]::Out.Write('first')",
                    Duration::from_secs(2),
                )
                .unwrap()
        });
        barrier.wait();
        while !manager
            .status("wc_shell_busy_win", "wc_sess_busy_win", PROJECT)
            .unwrap()
            .busy
        {
            thread::yield_now();
        }
        let busy = manager
            .exec(
                "wc_shell_busy_win",
                "wc_sess_busy_win",
                PROJECT,
                "[Console]::Out.Write('second')",
                Duration::from_secs(1),
            )
            .unwrap_err();
        assert_eq!(busy.code, "shell_busy");
        assert_eq!(worker.join().unwrap().stdout, "first");
    }

    #[test]
    fn close_is_idempotent_and_kills_owned_descendants() {
        let _guard = windows_process_test_guard();
        let temp = tempfile::tempdir().unwrap();
        let manager = PersistentShellManager::new(ShellLimits::default());
        manager
            .open(launch(
                temp.path(),
                "wc_shell_close_win",
                "wc_sess_close_win",
            ))
            .unwrap();
        let child = exec(
            &manager,
            "wc_shell_close_win",
            "wc_sess_close_win",
            "$p = Start-Process -FilePath 'powershell.exe' -ArgumentList '-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 30' -PassThru; [Console]::Out.Write($p.Id)",
        );
        let child_pid: u32 = child.stdout.parse().unwrap();
        assert!(process_alive(child_pid));
        let closed = manager
            .close(
                "wc_shell_close_win",
                "wc_sess_close_win",
                PROJECT,
                "explicit_close",
            )
            .unwrap();
        assert_eq!(closed.summary.state, ShellState::Closed);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && process_alive(child_pid) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !process_alive(child_pid),
            "owned child {child_pid} leaked after close"
        );
        let again = manager
            .close(
                "wc_shell_close_win",
                "wc_sess_close_win",
                PROJECT,
                "explicit_close",
            )
            .unwrap();
        assert!(again.already_closed);
    }
}
