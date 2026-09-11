use super::state::{
    JobLifecycleState, JobRecoveryPhase, JobRecoveryReason, RunnerRegistryInner, ShellJobLogState,
    ShellJobRecord,
};
use super::{
    now_ts, RunnerFeature, LIVE_JOB_STREAM_RETENTION_BYTES, MAX_QUEUED_REQUESTS_PER_RUNNER,
    ORDINARY_RESULT_STREAM_RETENTION_BYTES, RUNNER_ONLINE_WINDOW_SECS,
};
use std::collections::VecDeque;
use std::fmt;
pub use webcodex_core::audit_preview::{
    command_preview, process_preview, COMMAND_PREVIEW_MAX_CHARS,
};
use webcodex_core::runner_protocol::{
    RunnerJobResult, RunnerRequest, RunnerShellJobResult, ShellCommandExecutionState, ShellJobInfo,
    ShellJobStreamSnapshot,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PendingRequestEnqueueError {
    InvalidOperation { message: String },
    UnknownRunner { client_id: String },
    RunnerOffline { client_id: String },
    QueueFull { client_id: String, limit: usize },
}

impl fmt::Display for PendingRequestEnqueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOperation { message } => {
                write!(formatter, "invalid canonical Runner operation: {message}")
            }
            Self::UnknownRunner { client_id } => {
                write!(formatter, "unknown shell client: {client_id}")
            }
            Self::RunnerOffline { client_id } => write!(
                formatter,
                "runner {client_id} is offline (no keepalive within \
                 {RUNNER_ONLINE_WINDOW_SECS}s); reconnect the Runner before retrying"
            ),
            Self::QueueFull { client_id, limit } => write!(
                formatter,
                "too many pending requests for runner {client_id} (limit {limit})"
            ),
        }
    }
}

impl From<PendingRequestEnqueueError> for String {
    fn from(error: PendingRequestEnqueueError) -> Self {
        error.to_string()
    }
}

/// Bounded, body-free script summary used for activity and Session evidence.
/// It is presentation metadata only and can never be replayed as execution.
pub fn script_preview(language: &str, script_bytes: usize, arg_count: usize) -> String {
    format!("{language} script ({script_bytes} bytes, {arg_count} args)")
}

pub(super) fn request_preview(request: &RunnerRequest) -> String {
    if let Some(script) = request.script.as_ref() {
        script_preview(
            script.language.as_str(),
            script.script.len(),
            script.args.len(),
        )
    } else if let Some(process) = request.process.as_ref() {
        process_preview(&process.executable, process.args.iter().map(String::as_str))
    } else {
        command_preview(&request.command)
    }
}

#[cfg(test)]
mod command_preview_tests {
    use super::*;

    #[test]
    fn command_preview_redacts_secret_like_first_lines() {
        assert_eq!(
            command_preview("curl -H 'Authorization: Bearer example' https://example.invalid"),
            "[redacted]"
        );
        assert_eq!(command_preview("echo token=example"), "[redacted]");
        assert_eq!(command_preview("cargo test focused"), "cargo test focused");
    }

    #[test]
    fn process_preview_is_bounded_readable_and_never_an_execution_encoding() {
        let preview = process_preview(
            "git",
            ["status", "two words", "$(literal)", &"x".repeat(200)],
        );
        assert!(preview.starts_with("git status \"two words\" \"$(literal)\""));
        assert!(preview.chars().count() <= COMMAND_PREVIEW_MAX_CHARS + 1);
        assert!(preview.ends_with('…'));
        assert_eq!(
            process_preview("tool", ["Authorization: Bearer example"].into_iter()),
            "[redacted]"
        );
    }
}

#[cfg(test)]
mod select_lines_tests {
    use super::select_lines;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    // A default bounded tail returns only the last `tail_lines`, flags earlier
    // content, and points the cursor one past the last known line.
    #[test]
    fn tail_is_bounded_and_reports_next_cursor() {
        let value = (1..=10)
            .map(|n| format!("l{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (text, next, total, has_earlier) = select_lines(Some(&value), None, Some(3));
        assert_eq!(lines(&text.unwrap()), ["l8", "l9", "l10"]);
        assert_eq!(next, 11, "cursor is one past the last line");
        assert_eq!(total, 10);
        assert!(has_earlier, "earlier lines were skipped by the tail bound");
    }

    // Offset-only follow reads never re-emit consumed lines: reading from the
    // returned cursor yields nothing new, so a follower cannot loop on a tail.
    #[test]
    fn offset_follow_does_not_duplicate_consumed_lines() {
        let value = (1..=5)
            .map(|n| format!("l{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (first, next, _, has_earlier) = select_lines(Some(&value), Some(1), None);
        assert_eq!(lines(&first.unwrap()), ["l1", "l2", "l3", "l4", "l5"]);
        assert_eq!(next, 6);
        assert!(!has_earlier);
        // Following from the returned cursor returns no already-seen lines.
        let (second, next_again, _, _) = select_lines(Some(&value), Some(next), None);
        assert_eq!(
            second.unwrap(),
            "",
            "cursor past the end yields nothing new"
        );
        assert_eq!(next_again, 6, "cursor stays stable when drained");
        // A mid-stream offset returns only the forward slice.
        let (mid, _, _, mid_earlier) = select_lines(Some(&value), Some(4), None);
        assert_eq!(lines(&mid.unwrap()), ["l4", "l5"]);
        assert!(mid_earlier);
    }

    // When both bounds are supplied the tail wins, but the cursor still points
    // past the end so the next follow read drains rather than repeats the tail.
    #[test]
    fn tail_takes_precedence_but_cursor_still_advances() {
        let value = (1..=8)
            .map(|n| format!("l{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (text, next, _, _) = select_lines(Some(&value), Some(2), Some(3));
        assert_eq!(
            lines(&text.unwrap()),
            ["l6", "l7", "l8"],
            "tail_lines bounds the segment even when an offset is passed"
        );
        assert_eq!(next, 9);
        let (drained, _, _, _) = select_lines(Some(&value), Some(next), None);
        assert_eq!(
            drained.unwrap(),
            "",
            "following the cursor does not repeat the tail"
        );
    }
}

pub(super) fn retain_ordinary_result_stream(value: Option<String>) -> Option<String> {
    retain_result_stream_to(value, ORDINARY_RESULT_STREAM_RETENTION_BYTES)
}

fn retain_live_job_stream(value: Option<String>) -> Option<String> {
    retain_result_stream_to(value, LIVE_JOB_STREAM_RETENTION_BYTES)
}

pub(super) fn retain_result_stream_to(value: Option<String>, max_bytes: usize) -> Option<String> {
    value.map(|s| {
        if s.len() <= max_bytes {
            s
        } else {
            let mut start = s.len() - max_bytes;
            while start < s.len() && !s.is_char_boundary(start) {
                start += 1;
            }
            format!(
                "[output truncated to last {} bytes]\n{}",
                max_bytes,
                &s[start..]
            )
        }
    })
}

pub(super) fn job_view(job: &ShellJobRecord) -> ShellJobInfo {
    let now = now_ts();
    let elapsed_secs = if let Some(duration_ms) = job.duration_ms {
        Some(duration_ms / 1000)
    } else {
        job.started_at
            .map(|started_at| job.ended_at.unwrap_or(now).saturating_sub(started_at) as u64)
    };
    let result = if job.lifecycle.is_terminal() {
        Some(RunnerJobResult {
            shell: Some(RunnerShellJobResult {
                cwd: job.cwd.clone(),
                command_preview: job.command_preview.clone(),
                exit_code: job.exit_code,
                duration_ms: job.duration_ms,
                error: job.error.clone(),
            }),
        })
    } else {
        None
    };
    ShellJobInfo {
        job_id: job.job_id.clone(),
        request_id: job.request_id.clone(),
        client_id: job.client_id.clone(),
        kind: job.kind.clone(),
        project_id: job.project_id.clone(),
        session_id: job.session_id.clone(),
        ssh_resource: job.ssh_resource.clone(),
        cwd: job.cwd.clone(),
        project_cwd: job.project_cwd.clone(),
        purpose: job.purpose.clone(),
        shell: job.shell.clone(),
        command_preview: job.command_preview.clone(),
        status: job.public_status().to_string(),
        created_at: job.created_at,
        started_at: job.started_at,
        ended_at: job.ended_at,
        exit_code: job.exit_code,
        duration_ms: job.duration_ms,
        elapsed_secs,
        error: job.error.clone(),
        command_execution_state: job.command_execution_state,
        structured_execution: job.structured_execution.clone(),
        codex: job.codex.clone(),
        result,
        validation_progress: job.validation_progress.clone(),
        activity: job.activity,
        validation: job.validation.clone(),
        recovery_state: job.recovery.public_state().map(str::to_string),
        recovered_after_server_restart: job.recovery.recovered_after_server_restart,
        reconciled_at: job.recovery.reconciled_at,
        recovery_reason_code: job.recovery.public_reason().map(str::to_string),
        // General lifecycle views do not project log bodies, so they retain a
        // cursor-less legacy token. `job_log_for_auth` replaces this with a
        // cursor-aware v2 token for its frozen returned log snapshot.
        observation_token: webcodex_core::job_observation::JobObservationToken::new_legacy(
            job.job_id.clone(),
            job.observation.epoch.to_string(),
            job.observation
                .revision
                .load(std::sync::atomic::Ordering::Relaxed),
        )
        .ok()
        .map(|token| token.encode()),
        last_update_seq: Some(job.last_update_seq),
        stdout_retained_from_line: Some(job.stdout.first_retained_line),
        stderr_retained_from_line: Some(job.stderr.first_retained_line),
        stdout_log_truncated: job.stdout.truncated,
        stderr_log_truncated: job.stderr.truncated,
    }
}

#[cfg(test)]
pub(super) fn select_lines(
    value: Option<&String>,
    since_line: Option<usize>,
    tail_lines: Option<usize>,
) -> (Option<String>, usize, usize, bool) {
    let Some(value) = value else {
        return (Some(String::new()), since_line.unwrap_or(1), 0, false);
    };
    let lines = value.lines().collect::<Vec<_>>();
    if let Some(tail) = tail_lines.filter(|n| *n > 0) {
        let start = lines.len().saturating_sub(tail);
        let selected = lines[start..].join("\n");
        let text = if selected.is_empty() {
            selected
        } else {
            format!("{}\n", selected)
        };
        return (Some(text), lines.len() + 1, lines.len(), start > 0);
    }
    let start_line = since_line.unwrap_or(1).max(1);
    let start_idx = start_line.saturating_sub(1).min(lines.len());
    let selected = lines[start_idx..].join("\n");
    let text = if selected.is_empty() {
        selected
    } else {
        format!("{}\n", selected)
    };
    (Some(text), lines.len() + 1, lines.len(), start_idx > 0)
}

fn retained_line_count(value: &str) -> usize {
    value.lines().count()
}

pub(super) fn append_log_limited(target: &mut ShellJobLogState, chunk: Option<String>) {
    let Some(chunk) = chunk else {
        return;
    };
    target.tail.push_str(&chunk);
    if target.tail.len() > LIVE_JOB_STREAM_RETENTION_BYTES {
        let observed_next = target
            .first_retained_line
            .saturating_add(retained_line_count(&target.tail));
        let minimum_start = target.tail.len() - LIVE_JOB_STREAM_RETENTION_BYTES;
        if let Some(relative_newline) = target.tail[minimum_start..].find('\n') {
            let drop_end = minimum_start + relative_newline + 1;
            let dropped_lines = target.tail[..drop_end]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count();
            target.tail.drain(..drop_end);
            target.first_retained_line = target.first_retained_line.saturating_add(dropped_lines);
        } else {
            let mut start = minimum_start;
            while start < target.tail.len() && !target.tail.is_char_boundary(start) {
                start += 1;
            }
            let dropped_lines = target.tail[..start]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count();
            target.tail.drain(..start);
            target.first_retained_line = target.first_retained_line.saturating_add(dropped_lines);
        }
        if target.tail.is_empty() {
            target.first_retained_line = observed_next;
        }
        target.truncated = true;
    }
    target.next_line = target
        .first_retained_line
        .saturating_add(retained_line_count(&target.tail));
}

fn has_leading_result_retention_truncation_marker(value: &str) -> bool {
    if value.starts_with("[output truncated]\n") || value.starts_with("[...]\n") {
        return true;
    }

    let Some(rest) = value.strip_prefix("[output truncated to last ") else {
        return false;
    };
    let Some(newline) = rest.find('\n') else {
        return false;
    };
    let marker_tail = &rest[..newline];
    let Some(byte_count) = marker_tail.strip_suffix(" bytes]") else {
        return false;
    };
    !byte_count.is_empty() && byte_count.bytes().all(|byte| byte.is_ascii_digit())
}

pub(super) fn replace_log_limited(target: &mut ShellJobLogState, value: Option<String>) {
    let Some(value) = value else {
        return;
    };
    let value = retain_live_job_stream(Some(value)).unwrap_or_default();
    target.tail = value;
    target.first_retained_line = 1;
    target.next_line = 1usize.saturating_add(retained_line_count(&target.tail));
    target.truncated = has_leading_result_retention_truncation_marker(&target.tail);
}

#[cfg(test)]
mod replace_log_limited_tests {
    use super::*;

    #[test]
    fn recognizes_all_supported_result_retention_truncation_markers() {
        for marker in [
            "[output truncated to last 12000 bytes]\n",
            "[output truncated]\n",
            "[...]\n",
        ] {
            let mut log = ShellJobLogState::default();
            replace_log_limited(&mut log, Some(format!("{marker}retained\n")));
            assert!(log.truncated, "marker {marker:?}");
        }
    }

    #[test]
    fn ordinary_or_middle_marker_text_is_not_truncated() {
        for value in ["ordinary output\n", "ordinary\n[output truncated]\n"] {
            let mut log = ShellJobLogState::default();
            replace_log_limited(&mut log, Some(value.to_string()));
            assert!(!log.truncated, "value {value:?}");
        }
    }
}

pub(super) fn replace_log_from_snapshot(
    target: &mut ShellJobLogState,
    snapshot: &ShellJobStreamSnapshot,
) {
    target.tail = snapshot.tail.clone();
    target.first_retained_line = snapshot.first_retained_line;
    target.next_line = snapshot.next_line;
    target.truncated = snapshot.truncated;
}

pub(super) fn select_log_lines(
    log: &ShellJobLogState,
    since_line: Option<usize>,
    tail_lines: Option<usize>,
) -> (Option<String>, usize, usize, bool) {
    let lines = log.tail.lines().collect::<Vec<_>>();
    if let Some(tail) = tail_lines.filter(|n| *n > 0) {
        let start = lines.len().saturating_sub(tail);
        let selected = lines[start..].join("\n");
        let text = if selected.is_empty() {
            selected
        } else {
            format!("{}\n", selected)
        };
        return (
            Some(text),
            log.next_line,
            log.next_line.saturating_sub(1),
            log.first_retained_line > 1 || start > 0 || log.truncated,
        );
    }
    let requested = since_line
        .unwrap_or(log.first_retained_line)
        .max(log.first_retained_line);
    let start_idx = requested
        .saturating_sub(log.first_retained_line)
        .min(lines.len());
    let selected = lines[start_idx..].join("\n");
    let text = if selected.is_empty() {
        selected
    } else {
        format!("{}\n", selected)
    };
    (
        Some(text),
        log.next_line,
        log.next_line.saturating_sub(1),
        since_line.is_some_and(|line| line < log.first_retained_line)
            || start_idx > 0
            || log.truncated,
    )
}

pub(super) fn parse_job_lifecycle(status: &str) -> Result<JobLifecycleState, String> {
    JobLifecycleState::from_wire(status)
}

pub(super) fn is_final_job_status(status: &str) -> bool {
    JobLifecycleState::from_wire(status).is_ok_and(JobLifecycleState::is_terminal)
}

/// Record the first time this Server process observes a Job in a terminal
/// state. This internal lifecycle timestamp is deliberately independent of
/// the Runner-reported `ended_at` execution timestamp. Replays and duplicate
/// terminal transitions are idempotent.
pub(super) fn observe_job_terminal(job: &mut ShellJobRecord, now: i64) {
    if job.lifecycle.is_terminal() && job.observation.terminal_observed_at.is_none() {
        job.observation.terminal_observed_at = Some(now);
    }
}

/// Broadcast an observable update for a job. Any mutation to a job's public
/// snapshot or `last_update_seq` must call this while holding the registry
/// mutex, so bounded `job_log`/`job_tail` waiters are woken to re-read the
/// authoritative snapshot. `notify_waiters` (not `notify_one`) so that every
/// concurrent waiter observes the update; waiters re-check the snapshot after
/// every wake, so spurious broadcasts are harmless.
pub(super) fn notify_job_update(job: &ShellJobRecord) {
    use std::sync::atomic::Ordering;
    job.observation.revision.fetch_add(1, Ordering::Relaxed);
    job.observation.notify.notify_waiters();
}

pub(super) fn is_runner_active_job_status(status: &str) -> bool {
    JobLifecycleState::from_wire(status).is_ok_and(JobLifecycleState::is_runner_active)
}

pub(super) fn begin_job_recovery(job: &mut ShellJobRecord, now: i64, reason: JobRecoveryReason) {
    if job.lifecycle.is_terminal() || job.lifecycle == JobLifecycleState::Queued {
        return;
    }
    if !job.recovery.recovering() {
        job.recovery.recovering_since = Some(now);
    }
    job.recovery.phase = Some(JobRecoveryPhase::Recovering);
    job.recovery.reason = Some(reason);
    job.ended_at = None;
    job.activity = None;
    notify_job_update(job);
}

pub(super) fn mark_job_lost(
    job: &mut ShellJobRecord,
    now: i64,
    reason: JobRecoveryReason,
    message: &str,
) {
    if job.lifecycle.is_terminal() {
        return;
    }
    job.lifecycle = JobLifecycleState::Lost;
    job.activity = None;
    observe_job_terminal(job, now);
    if job.ended_at.is_none() {
        job.ended_at = Some(now);
    }
    job.error = Some(message.to_string());
    if job.structured_execution.is_some() {
        job.command_execution_state = Some(if job.started_at.is_some() {
            ShellCommandExecutionState::OutcomeUnknown
        } else {
            ShellCommandExecutionState::NotStarted
        });
    }
    job.recovery.phase = reason
        .implies_lost_after_reconcile()
        .then_some(JobRecoveryPhase::LostAfterReconcile);
    job.recovery.reason = Some(reason);
    job.recovery.recovering_since = None;
    notify_job_update(job);
}

fn runner_is_connected_locked(inner: &RunnerRegistryInner, client_id: &str) -> bool {
    inner
        .runners
        .get(client_id)
        .map(|runner| now_ts().saturating_sub(runner.last_seen) <= RUNNER_ONLINE_WINDOW_SECS)
        .unwrap_or(false)
}

pub(super) fn offline_last_seen(now: i64) -> i64 {
    now.saturating_sub(RUNNER_ONLINE_WINDOW_SECS.saturating_add(1))
}

/// Verify that `client_id` exists and that `agent_instance_id` matches the
/// instance that currently holds the lease for it. A replaced instance (the
/// previous process after an explicit registration takeover) is rejected so it
/// can no longer poll or submit results.
/// Callers must already hold `inner`.
pub(super) fn assert_active_instance_locked(
    inner: &RunnerRegistryInner,
    client_id: &str,
    runner_instance_id: &str,
) -> Result<(), String> {
    let Some(runner) = inner.runners.get(client_id) else {
        return Err(format!("unknown shell client: {}", client_id));
    };
    if runner.runner_instance_id != runner_instance_id {
        return Err(format!(
            "runner {} is no longer the active instance (stale or replaced)",
            client_id
        ));
    }
    Ok(())
}

/// Reject enqueue when a runner's pending queue has reached
/// `MAX_QUEUED_REQUESTS_PER_RUNNER`. Callers must already hold `inner`.
pub(super) fn ensure_queue_capacity_locked(
    inner: &RunnerRegistryInner,
    client_id: &str,
) -> Result<(), PendingRequestEnqueueError> {
    let len = inner
        .queues_by_runner
        .get(client_id)
        .map(VecDeque::len)
        .unwrap_or(0);
    if len >= MAX_QUEUED_REQUESTS_PER_RUNNER {
        return Err(PendingRequestEnqueueError::QueueFull {
            client_id: client_id.to_string(),
            limit: MAX_QUEUED_REQUESTS_PER_RUNNER,
        });
    }
    Ok(())
}

/// Ensure a request target exists and is currently online before enqueueing
/// work for the Runner request pump. Callers must already hold `inner`.
///
/// Online is defined by `RUNNER_ONLINE_WINDOW_SECS` against `last_seen`. Without
/// this gate, a registered-but-disconnected Runner still accepts enqueues that
/// can only fail after the caller's wait timeout (or pile up until
/// `MAX_QUEUED_REQUESTS_PER_RUNNER` and then permanently reject new work for
/// that runner until process restart) — a major amplifier of MCP "no reply".
pub(super) fn ensure_dispatch_supported_locked(
    inner: &RunnerRegistryInner,
    client_id: &str,
) -> Result<(), PendingRequestEnqueueError> {
    if !inner.runners.contains_key(client_id) {
        return Err(PendingRequestEnqueueError::UnknownRunner {
            client_id: client_id.to_string(),
        });
    }
    if !runner_is_connected_locked(inner, client_id) {
        return Err(PendingRequestEnqueueError::RunnerOffline {
            client_id: client_id.to_string(),
        });
    }
    Ok(())
}

pub(super) fn refresh_job_status_locked(inner: &mut RunnerRegistryInner, job_id: &str) {
    let Some(job) = inner.jobs_by_id.get(job_id) else {
        return;
    };
    if job.lifecycle.is_terminal() || !job.lifecycle.is_runner_active() {
        return;
    }
    if job.recovery_active() {
        let expired = job.recovery.recovering_since.is_some_and(|since| {
            now_ts().saturating_sub(since) >= super::job_recovery_grace_secs()
        });
        if expired {
            if let Some(job) = inner.jobs_by_id.get_mut(job_id) {
                mark_job_lost(
                    job,
                    now_ts(),
                    JobRecoveryReason::RunnerRecoveryDeadlineExceeded,
                    "runner did not reconcile the job before the recovery deadline",
                );
            }
        }
        return;
    }
    let client_id = job.client_id.clone();
    if runner_is_connected_locked(inner, &client_id) {
        return;
    }
    let recoverable = inner.runners.get(&client_id).is_some_and(|runner| {
        runner
            .runner_features
            .supports(RunnerFeature::JobStateReconciliation)
    });
    if let Some(job) = inner.jobs_by_id.get_mut(job_id) {
        if recoverable {
            begin_job_recovery(job, now_ts(), JobRecoveryReason::RunnerTransportStale);
        } else {
            mark_job_lost(
                job,
                now_ts(),
                JobRecoveryReason::RunnerDisconnectedWithoutReconciliation,
                "runner went stale while job was running",
            );
        }
    }
}
