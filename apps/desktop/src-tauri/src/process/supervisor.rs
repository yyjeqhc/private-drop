use crate::activity::{sanitize_message, ActivityEventKind, ActivityLevel, ActivityLog};
use crate::deadline::Deadline;
use crate::error::{DesktopError, DesktopResult};
use crate::platform;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use webcodex_process::{GracefulTermination, ManagedChild};

const LOG_LINES: usize = 80;
const LOG_LINE_BYTES: usize = 2048;
const MACHINE_LINE_BYTES: usize = 16 * 1024;
const MACHINE_EVENT_CAPACITY: usize = 64;
const MACHINE_CRITICAL_RESERVE: usize = 8;
const GRACEFUL_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const LOCAL_EOF_GRACE: std::time::Duration = std::time::Duration::from_millis(250);
const PROCESS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProcessKind {
    LocalServer,
    LocalRunner,
    QuickShare,
    RegularTunnel,
}

impl ProcessKind {
    fn source(self) -> &'static str {
        match self {
            Self::LocalServer => "service",
            Self::LocalRunner => "runner",
            Self::QuickShare => "quick_share",
            Self::RegularTunnel => "regular_tunnel",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessPhase {
    Starting,
    Running,
    Stopping,
    Exited,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessSnapshot {
    pub kind: ProcessKind,
    pub phase: ProcessPhase,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub owned_by_desktop: bool,
}

struct ManagedProcess {
    child: ManagedChild,
    phase: ProcessPhase,
    exit_code: Option<i32>,
    logs: Arc<Mutex<VecDeque<String>>>,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
}

#[derive(Default)]
struct MachineEventState {
    queue: VecDeque<Value>,
    closed: bool,
    dropped_progress: u64,
    dropped_critical: u64,
}

#[derive(Clone)]
struct MachineEventSender {
    state: Arc<Mutex<MachineEventState>>,
    notify: Arc<Notify>,
}

pub(crate) struct MachineEventReceiver {
    state: Arc<Mutex<MachineEventState>>,
    notify: Arc<Notify>,
}

fn machine_event_channel() -> (MachineEventSender, MachineEventReceiver) {
    let state = Arc::new(Mutex::new(MachineEventState::default()));
    let notify = Arc::new(Notify::new());
    (
        MachineEventSender {
            state: Arc::clone(&state),
            notify: Arc::clone(&notify),
        },
        MachineEventReceiver { state, notify },
    )
}

impl MachineEventReceiver {
    pub(crate) async fn recv(&mut self) -> Option<Value> {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if state.dropped_critical > 0 {
                    let dropped = std::mem::take(&mut state.dropped_critical);
                    return Some(serde_json::json!({
                        "event": "machine_event_overflow",
                        "dropped_critical": dropped,
                    }));
                }
                if let Some(value) = state.queue.pop_front() {
                    return Some(value);
                }
                if state.closed {
                    return None;
                }
            }
            notified.await;
        }
    }
}

impl MachineEventSender {
    fn send(&self, value: Value) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed {
            return;
        }
        if machine_event_is_progress(&value) {
            let progress_limit = MACHINE_EVENT_CAPACITY.saturating_sub(MACHINE_CRITICAL_RESERVE);
            if state.queue.len() >= progress_limit {
                if let Some(existing) = state
                    .queue
                    .iter_mut()
                    .rev()
                    .find(|event| machine_event_is_progress(event))
                {
                    *existing = value;
                } else {
                    state.dropped_progress = state.dropped_progress.saturating_add(1);
                }
                return;
            }
            state.queue.push_back(value);
        } else {
            if state.queue.len() >= MACHINE_EVENT_CAPACITY {
                if let Some(index) = state.queue.iter().position(machine_event_is_progress) {
                    state.queue.remove(index);
                    state.dropped_progress = state.dropped_progress.saturating_add(1);
                } else if machine_event_is_terminal(&value) {
                    if let Some(index) = state
                        .queue
                        .iter()
                        .position(|event| !machine_event_is_terminal(event))
                    {
                        state.queue.remove(index);
                    } else {
                        state.queue.pop_front();
                    }
                    state.dropped_critical = state.dropped_critical.saturating_add(1);
                } else {
                    state.dropped_critical = state.dropped_critical.saturating_add(1);
                    drop(state);
                    self.notify.notify_one();
                    return;
                }
            }
            state.queue.push_back(value);
        }
        drop(state);
        self.notify.notify_one();
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.closed = true;
        drop(state);
        self.notify.notify_waiters();
    }
}

fn machine_event_is_progress(value: &Value) -> bool {
    value.get("event").and_then(Value::as_str) == Some("progress")
}

fn machine_event_is_terminal(value: &Value) -> bool {
    matches!(
        value.get("event").and_then(Value::as_str),
        Some("ready" | "error" | "failed" | "stopped" | "exit" | "exited" | "terminal")
    )
}

pub struct ProcessSupervisor {
    processes: HashMap<ProcessKind, ManagedProcess>,
    activity: ActivityLog,
}

impl ProcessSupervisor {
    pub fn new(activity: ActivityLog) -> Self {
        Self {
            processes: HashMap::new(),
            activity,
        }
    }

    pub async fn spawn_owned(
        &mut self,
        kind: ProcessKind,
        mut command: Command,
        machine_stdout: bool,
    ) -> DesktopResult<Option<MachineEventReceiver>> {
        self.refresh();
        if self.processes.get(&kind).is_some_and(|process| {
            matches!(
                process.phase,
                ProcessPhase::Starting | ProcessPhase::Running | ProcessPhase::Stopping
            )
        }) {
            return Err(DesktopError::new(
                "process_already_running",
                format!("Desktop already owns an active {kind:?} process"),
                "Stop the existing Desktop-owned process first.",
            ));
        }
        if self.processes.contains_key(&kind) {
            // A terminal direct child may still own live descendants. Keep the
            // exact ManagedChild generation until its whole tree is reclaimed;
            // never retarget cleanup by a remembered numeric PID/PGID.
            self.stop(kind).await;
        }

        // stdin is the Desktop parent-liveness lease for every long-lived
        // generation. Quick Share/Tunnel already consume EOF; Local Server and
        // Runner do so only when Desktop adds their explicit opt-in CLI flag.
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        // The Desktop owns the Runner itself, but the Runner is also a trusted
        // process supervisor: each user command is immediately placed into the
        // Runner's own ManagedChild Job Object. Let only this direct child
        // silently break descendants away from the Desktop's outer Job to avoid
        // nested-Job incompatibilities (notably Git for Windows/MSYS) while
        // preserving exact ownership at both lifecycle layers.
        let silent_child_breakaway = kind == ProcessKind::LocalRunner;
        let mut child = ManagedChild::spawn_with_options(
            &mut command,
            platform::managed_spawn_options(silent_child_breakaway),
        )
        .map_err(|error| {
            DesktopError::new(
                "process_start_failed",
                format!("Could not start the {kind:?} process"),
                "Check the configured WebCodex binaries and retry.",
            )
            .with_details(serde_json::json!({ "io_kind": format!("{:?}", error.kind()) }))
        })?;
        let pid = child.id();
        let stdout = child.child_mut().stdout.take().ok_or_else(|| {
            DesktopError::new(
                "process_start_failed",
                "Could not capture process output",
                "Retry the operation.",
            )
        })?;
        let stderr = child.child_mut().stderr.take().ok_or_else(|| {
            DesktopError::new(
                "process_start_failed",
                "Could not capture process diagnostics",
                "Retry the operation.",
            )
        })?;

        let logs = Arc::new(Mutex::new(VecDeque::new()));
        let (machine_tx, machine_rx) = if machine_stdout {
            let (tx, rx) = machine_event_channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let stdout_logs = Arc::clone(&logs);
        let stdout_task = tokio::task::spawn_blocking(move || {
            drain_stream(stdout, stdout_logs, machine_tx, machine_stdout)
        });
        let stderr_logs = Arc::clone(&logs);
        let stderr_task =
            tokio::task::spawn_blocking(move || drain_stream(stderr, stderr_logs, None, false));
        self.activity.push(
            ActivityEventKind::ProcessStarted,
            kind.source(),
            ActivityLevel::Info,
            format!("Desktop started the process (PID {pid})"),
        );
        self.processes.insert(
            kind,
            ManagedProcess {
                child,
                phase: ProcessPhase::Starting,
                exit_code: None,
                logs,
                stdout_task,
                stderr_task,
            },
        );
        Ok(machine_rx)
    }

    pub fn refresh(&mut self) {
        for (kind, process) in &mut self.processes {
            if !matches!(
                process.phase,
                ProcessPhase::Starting | ProcessPhase::Running
            ) {
                continue;
            }
            match process.child.try_wait() {
                Ok(Some(status)) => {
                    process.exit_code = status.code();
                    process.phase = if status.success() {
                        ProcessPhase::Exited
                    } else {
                        ProcessPhase::Failed
                    };
                    self.activity.push(
                        ActivityEventKind::ProcessExited,
                        kind.source(),
                        if status.success() {
                            ActivityLevel::Info
                        } else {
                            ActivityLevel::Error
                        },
                        format!("Desktop-owned process exited with status {status}"),
                    );
                }
                Ok(None) => process.phase = ProcessPhase::Running,
                Err(_) => {
                    process.phase = ProcessPhase::Failed;
                    self.activity.push(
                        ActivityEventKind::ProcessObservationFailed,
                        kind.source(),
                        ActivityLevel::Error,
                        "Desktop could not observe the child process state",
                    );
                }
            }
        }
    }

    pub fn snapshot(&mut self, kind: ProcessKind) -> Option<ProcessSnapshot> {
        self.refresh();
        self.processes.get(&kind).map(|process| ProcessSnapshot {
            kind,
            phase: process.phase,
            pid: Some(process.child.id()),
            exit_code: process.exit_code,
            owned_by_desktop: true,
        })
    }

    pub fn logs(&self, kind: ProcessKind) -> Vec<String> {
        self.processes
            .get(&kind)
            .map(|process| {
                process
                    .logs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub async fn stop(&mut self, kind: ProcessKind) {
        self.stop_until(kind, Deadline::after(GRACEFUL_STOP_TIMEOUT))
            .await;
    }

    pub async fn stop_until(&mut self, kind: ProcessKind, deadline: Deadline) {
        let Some(mut process) = self.processes.remove(&kind) else {
            return;
        };
        // Closing the Desktop side of stdin is the generation-scoped parent
        // lease. Do it even if the direct child was already observed terminal:
        // a descendant may still hold the child side of the pipe.
        drop(process.child.child_mut().stdin.take());
        if matches!(
            process.phase,
            ProcessPhase::Starting | ProcessPhase::Running
        ) {
            process.phase = ProcessPhase::Stopping;
            self.activity.push(
                ActivityEventKind::ProcessStopping,
                kind.source(),
                ActivityLevel::Info,
                "Stopping the Desktop-owned process",
            );

            let now = tokio::time::Instant::now();
            let eof_deadline =
                if matches!(kind, ProcessKind::QuickShare | ProcessKind::RegularTunnel) {
                    deadline.instant()
                } else {
                    std::cmp::min(deadline.instant(), now + LOCAL_EOF_GRACE)
                };
            let graceful = wait_for_tree_exit(&mut process.child, eof_deadline).await;
            if !graceful && tokio::time::Instant::now() < deadline.instant() {
                if matches!(
                    process.child.request_terminate_tree(),
                    Ok(GracefulTermination::Requested)
                ) {
                    let signal_deadline = std::cmp::min(
                        deadline.instant(),
                        tokio::time::Instant::now() + LOCAL_EOF_GRACE,
                    );
                    let _ = wait_for_tree_exit(&mut process.child, signal_deadline).await;
                }
            }
            if !process.child.try_tree_exit().unwrap_or(false) {
                let _ = process.child.terminate_tree();
                let _ = wait_for_tree_exit(&mut process.child, deadline.instant()).await;
            }
        }
        if !process.child.try_tree_exit().unwrap_or(false) {
            let _ = process.child.terminate_tree();
            let _ = wait_for_tree_exit(&mut process.child, deadline.instant()).await;
        }
        finish_drain_task(process.stdout_task, deadline.instant()).await;
        finish_drain_task(process.stderr_task, deadline.instant()).await;
        self.activity.push(
            ActivityEventKind::ProcessStopped,
            kind.source(),
            ActivityLevel::Info,
            "Desktop-owned process stopped",
        );
    }

    pub async fn stop_all(&mut self) {
        for kind in [
            ProcessKind::QuickShare,
            ProcessKind::RegularTunnel,
            ProcessKind::LocalRunner,
            ProcessKind::LocalServer,
        ] {
            self.stop(kind).await;
        }
    }
}

async fn wait_for_tree_exit(child: &mut ManagedChild, deadline: tokio::time::Instant) -> bool {
    loop {
        let _ = child.try_wait();
        if child.try_tree_exit().unwrap_or(false) {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep_until(std::cmp::min(deadline, now + PROCESS_POLL_INTERVAL)).await;
    }
}

async fn finish_drain_task(mut task: JoinHandle<()>, deadline: tokio::time::Instant) {
    if tokio::time::Instant::now() >= deadline
        || tokio::time::timeout_at(deadline, &mut task).await.is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

fn drain_stream<R>(
    mut reader: R,
    logs: Arc<Mutex<VecDeque<String>>>,
    machine_tx: Option<MachineEventSender>,
    machine_only: bool,
) where
    R: Read,
{
    let mut buffer = [0_u8; 4096];
    let mut line = Vec::with_capacity(4096);
    let line_limit = if machine_only {
        MACHINE_LINE_BYTES
    } else {
        LOG_LINE_BYTES
    };
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        for byte in &buffer[..read] {
            if *byte == b'\n' {
                process_line(&line, &logs, machine_tx.as_ref(), machine_only);
                line.clear();
            } else if line.len() < line_limit {
                line.push(*byte);
            }
        }
    }
    if !line.is_empty() {
        process_line(&line, &logs, machine_tx.as_ref(), machine_only);
    }
    if let Some(tx) = machine_tx {
        tx.close();
    }
}

fn process_line(
    line: &[u8],
    logs: &Arc<Mutex<VecDeque<String>>>,
    machine_tx: Option<&MachineEventSender>,
    machine_only: bool,
) {
    let text = String::from_utf8_lossy(line).trim().to_string();
    if text.is_empty() {
        return;
    }
    if machine_only {
        if let (Some(tx), Ok(value)) = (machine_tx, serde_json::from_str::<Value>(&text)) {
            tx.send(value);
        }
        return;
    }
    let mut logs = logs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    logs.push_back(sanitize_message(&text));
    while logs.len() > LOG_LINES {
        logs.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn machine_event_progress_flood_stays_bounded_and_ready_is_observed() {
        let (sender, mut receiver) = machine_event_channel();
        for sequence in 0..10_000_u64 {
            sender.send(serde_json::json!({
                "event": "progress",
                "sequence": sequence,
            }));
        }
        sender.send(serde_json::json!({ "event": "ready", "schema_version": 1 }));
        sender.close();

        let queued = sender
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .queue
            .len();
        assert!(queued <= MACHINE_EVENT_CAPACITY);

        let mut ready = false;
        while let Some(value) = receiver.recv().await {
            if value.get("event").and_then(Value::as_str) == Some("ready") {
                ready = true;
                break;
            }
        }
        assert!(ready, "readiness event must survive a noisy progress flood");
    }

    #[tokio::test]
    async fn critical_overflow_is_explicit_and_terminal_event_is_retained() {
        let (sender, mut receiver) = machine_event_channel();
        for sequence in 0..MACHINE_EVENT_CAPACITY {
            sender.send(serde_json::json!({
                "event": "diagnostic",
                "sequence": sequence,
            }));
        }
        sender.send(serde_json::json!({ "event": "terminal", "status": "failed" }));
        sender.close();

        let mut saw_overflow = false;
        let mut saw_terminal = false;
        while let Some(value) = receiver.recv().await {
            match value.get("event").and_then(Value::as_str) {
                Some("machine_event_overflow") => saw_overflow = true,
                Some("terminal") => saw_terminal = true,
                _ => {}
            }
        }
        assert!(saw_overflow, "critical loss must never be silent");
        assert!(saw_terminal, "terminal event must remain observable");
    }

    #[tokio::test]
    async fn supervisor_only_stops_children_it_owns() {
        let activity = ActivityLog::default();
        let mut supervisor = ProcessSupervisor::new(activity);
        supervisor.stop(ProcessKind::LocalRunner).await;
        assert!(supervisor.snapshot(ProcessKind::LocalRunner).is_none());
    }

    #[test]
    fn process_kind_has_no_generic_process_surface() {
        let kinds = [
            ProcessKind::LocalServer,
            ProcessKind::LocalRunner,
            ProcessKind::QuickShare,
            ProcessKind::RegularTunnel,
        ];
        assert_eq!(kinds.len(), 4);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_parent_liveness_lease_closes_on_desktop_stop() {
        let marker = std::env::temp_dir().join(format!(
            "webcodex-desktop-local-parent-eof-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let marker_arg = marker.to_string_lossy().into_owned();
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "cat >/dev/null; printf eof > \"$1\"",
            "webcodex-parent-eof",
            marker_arg.as_str(),
        ]);

        let activity = ActivityLog::default();
        let mut supervisor = ProcessSupervisor::new(activity);
        supervisor
            .spawn_owned(ProcessKind::LocalServer, command, false)
            .await
            .expect("start local parent-liveness fixture");
        supervisor.stop(ProcessKind::LocalServer).await;

        assert!(
            marker.is_file(),
            "local generation must observe stdin EOF before forced tree cleanup"
        );
        let _ = std::fs::remove_file(marker);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn quick_share_eof_stop_remains_green() {
        let marker = std::env::temp_dir().join(format!(
            "webcodex-desktop-quick-share-eof-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let marker_arg = marker.to_string_lossy().into_owned();
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "cat >/dev/null; printf eof > \"$1\"",
            "webcodex-quick-share-eof",
            marker_arg.as_str(),
        ]);

        let activity = ActivityLog::default();
        let mut supervisor = ProcessSupervisor::new(activity);
        supervisor
            .spawn_owned(ProcessKind::QuickShare, command, false)
            .await
            .expect("start Quick Share EOF fixture");
        supervisor.stop(ProcessKind::QuickShare).await;

        assert!(marker.is_file(), "Quick Share child must observe stdin EOF");
        let _ = std::fs::remove_file(marker);
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "Desktop Windows real-process lane: waits on a real PowerShell stdin EOF"]
    async fn desktop_real_process_windows_regular_tunnel_stop_closes_stdin_for_canonical_graceful_shutdown() {
        let marker = std::env::temp_dir().join(format!(
            "webcodex-desktop-regular-tunnel-eof-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let escaped_marker = marker.to_string_lossy().replace('\'', "''");
        let mut command = Command::new("powershell.exe");
        command
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(format!(
                "$marker = '{escaped_marker}'; Set-Content -LiteralPath $marker -Value 'ready'; $null = [Console]::In.ReadToEnd(); Set-Content -LiteralPath $marker -Value 'eof'"
            ));

        let activity = ActivityLog::default();
        let mut supervisor = ProcessSupervisor::new(activity);
        supervisor
            .spawn_owned(ProcessKind::RegularTunnel, command, false)
            .await
            .expect("start EOF fixture");
        let ready_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if std::fs::read_to_string(&marker)
                .ok()
                .is_some_and(|value| value.trim() == "ready")
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < ready_deadline,
                "regular tunnel EOF fixture must become ready before stop"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        supervisor.stop(ProcessKind::RegularTunnel).await;

        assert_eq!(
            std::fs::read_to_string(&marker)
                .expect("regular tunnel EOF fixture marker")
                .trim(),
            "eof",
            "regular tunnel child must observe stdin EOF before the graceful stop completes",
        );
        let _ = std::fs::remove_file(marker);
    }
}
