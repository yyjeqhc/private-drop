use super::helpers::{bounded_tail, COMMAND_STDIO_TAIL_CHARS};
use crate::auth::AuthContext;
use crate::runner_http::RunnerRegistry;
use crate::runner_protocol::{
    ShellJobInfo, STRUCTURED_EXECUTION_TIMEOUT_DEFAULT_SECS, STRUCTURED_EXECUTION_TIMEOUT_MAX_SECS,
    STRUCTURED_EXECUTION_TIMEOUT_MIN_SECS,
};
use std::sync::Arc;
use std::time::Duration;
use webcodex_core::runner_job_lifecycle::RunnerJobLifecycle;
use webcodex_runner_registry::RunnerAccess;

pub(crate) const STRUCTURED_EXECUTION_SYNC_WAIT_SECS: u64 = 10;
pub(crate) const INITIAL_JOB_HANDOFF_TAIL_LINES: usize = 40;
pub(crate) use webcodex_core::runtime_contract::STRUCTURED_EXECUTION_SYNC_WAIT_MAX_SECS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StructuredExecutionBudget {
    pub(crate) effective_timeout_secs: u64,
    pub(crate) sync_wait_secs: u64,
}

impl StructuredExecutionBudget {
    pub(crate) fn resolve(timeout_secs: Option<u64>) -> Result<Self, String> {
        Self::resolve_with_sync_wait(timeout_secs, None)
    }

    pub(crate) fn resolve_with_sync_wait(
        timeout_secs: Option<u64>,
        sync_wait_secs: Option<u64>,
    ) -> Result<Self, String> {
        let effective_timeout_secs =
            timeout_secs.unwrap_or(STRUCTURED_EXECUTION_TIMEOUT_DEFAULT_SECS);
        if !(STRUCTURED_EXECUTION_TIMEOUT_MIN_SECS..=STRUCTURED_EXECUTION_TIMEOUT_MAX_SECS)
            .contains(&effective_timeout_secs)
        {
            return Err(format!(
                "timeout_secs must be between {STRUCTURED_EXECUTION_TIMEOUT_MIN_SECS} and {STRUCTURED_EXECUTION_TIMEOUT_MAX_SECS}"
            ));
        }
        let sync_wait_secs = match sync_wait_secs {
            Some(sync_wait_secs) => {
                if !(1..=STRUCTURED_EXECUTION_SYNC_WAIT_MAX_SECS).contains(&sync_wait_secs) {
                    return Err(format!(
                        "sync_wait_secs must be between 1 and {STRUCTURED_EXECUTION_SYNC_WAIT_MAX_SECS}"
                    ));
                }
                if sync_wait_secs > effective_timeout_secs {
                    return Err(format!(
                        "sync_wait_secs ({sync_wait_secs}) must not exceed effective timeout_secs ({effective_timeout_secs})"
                    ));
                }
                sync_wait_secs
            }
            None => STRUCTURED_EXECUTION_SYNC_WAIT_SECS.min(effective_timeout_secs),
        };
        Ok(Self {
            effective_timeout_secs,
            sync_wait_secs,
        })
    }
}

pub(crate) enum HiddenStructuredJobWait {
    Terminal {
        job: ShellJobInfo,
        stdout: String,
        stderr: String,
    },
    Continued {
        observation: StructuredJobObservation,
        execution_state: &'static str,
        command_started: bool,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct StructuredJobObservation {
    pub(crate) job: ShellJobInfo,
    pub(crate) stdout_tail: String,
    pub(crate) stderr_tail: String,
    pub(crate) stdout_lines: usize,
    pub(crate) stderr_lines: usize,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_truncated: bool,
}

pub(crate) async fn structured_job_observation(
    clients: &RunnerRegistry,
    access: Option<&RunnerAccess>,
    job_id: &str,
) -> Result<StructuredJobObservation, String> {
    let (job, stdout, stderr, next_stdout_line, next_stderr_line) = clients
        .hidden_job_log_for_auth(access, job_id, Some(INITIAL_JOB_HANDOFF_TAIL_LINES))
        .await?;
    let stdout = stdout.unwrap_or_default();
    let stderr = stderr.unwrap_or_default();
    let stdout_lines = next_stdout_line.saturating_sub(1);
    let stderr_lines = next_stderr_line.saturating_sub(1);
    let (stdout_tail, stdout_char_truncated) = bounded_tail(&stdout, COMMAND_STDIO_TAIL_CHARS);
    let (stderr_tail, stderr_char_truncated) = bounded_tail(&stderr, COMMAND_STDIO_TAIL_CHARS);
    let stdout_truncated = job.stdout_log_truncated
        || job.stdout_retained_from_line.is_some_and(|line| line > 1)
        || stdout.lines().count() < stdout_lines
        || stdout_char_truncated;
    let stderr_truncated = job.stderr_log_truncated
        || job.stderr_retained_from_line.is_some_and(|line| line > 1)
        || stderr.lines().count() < stderr_lines
        || stderr_char_truncated;
    Ok(StructuredJobObservation {
        job,
        stdout_tail,
        stderr_tail,
        stdout_lines,
        stderr_lines,
        stdout_truncated,
        stderr_truncated,
    })
}

pub(crate) async fn await_hidden_structured_job(
    clients: Arc<RunnerRegistry>,
    job_id: String,
    sync_wait: Duration,
    auth: Option<AuthContext>,
) -> Result<HiddenStructuredJobWait, String> {
    let access = crate::runner_http::runner_access_from_auth(auth.as_ref());
    let mut guard = HiddenJobCleanupGuard::new(clients.clone(), job_id.clone(), access.clone());
    let deadline = std::time::Instant::now() + sync_wait;
    loop {
        if let Ok(job) = clients
            .get_hidden_job_for_auth(access.as_ref(), &job_id)
            .await
        {
            if crate::tool_runtime::jobs::is_terminal_job_status(&job.status) {
                let terminal = hidden_terminal_snapshot(&clients, access.as_ref(), &job_id).await?;
                guard.disarm();
                return Ok(terminal);
            }
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let promoted = clients.promote_hidden_job(&job_id).await?;
    if crate::tool_runtime::jobs::is_terminal_job_status(&promoted.status) {
        let terminal = hidden_terminal_snapshot(&clients, access.as_ref(), &job_id).await?;
        guard.disarm();
        return Ok(terminal);
    }
    let observation = structured_job_observation(&clients, access.as_ref(), &job_id).await?;
    if crate::tool_runtime::jobs::is_terminal_job_status(&observation.job.status) {
        let terminal = hidden_terminal_snapshot(&clients, access.as_ref(), &job_id).await?;
        guard.disarm();
        return Ok(terminal);
    }
    let (execution_state, command_started) = continued_execution_state(
        observation.job.status.as_str(),
        observation.job.started_at.is_some(),
    );
    guard.disarm();
    Ok(HiddenStructuredJobWait::Continued {
        observation,
        execution_state,
        command_started,
    })
}

fn continued_execution_state(status: &str, started: bool) -> (&'static str, bool) {
    // Recovery is a Server observation overlay, not Runner lifecycle truth, and
    // does not prove that execution is presently running.
    if status == "recovering" {
        return ("outcome_unknown", true);
    }
    match RunnerJobLifecycle::from_wire(status).ok() {
        Some(
            RunnerJobLifecycle::Queued
            | RunnerJobLifecycle::RunnerQueued
            | RunnerJobLifecycle::StartedLegacy,
        ) if started => ("running", true),
        Some(
            RunnerJobLifecycle::Queued
            | RunnerJobLifecycle::RunnerQueued
            | RunnerJobLifecycle::StartedLegacy,
        ) => ("queued", false),
        Some(RunnerJobLifecycle::Running) => ("running", true),
        Some(RunnerJobLifecycle::StopRequested) if started => ("running", true),
        Some(RunnerJobLifecycle::StopRequested) => ("queued", false),
        _ => ("outcome_unknown", true),
    }
}

async fn hidden_terminal_snapshot(
    clients: &RunnerRegistry,
    access: Option<&RunnerAccess>,
    job_id: &str,
) -> Result<HiddenStructuredJobWait, String> {
    let (job, stdout, stderr, _, _) = clients
        .hidden_job_log_for_auth(access, job_id, None)
        .await?;
    Ok(HiddenStructuredJobWait::Terminal {
        job,
        stdout: stdout.unwrap_or_default(),
        stderr: stderr.unwrap_or_default(),
    })
}

/// Cancels an initially hidden Job if the initiating future is dropped before
/// it can return either a terminal projection or a public continuation.
struct HiddenJobCleanupGuard {
    clients: Arc<RunnerRegistry>,
    job_id: String,
    access: Option<RunnerAccess>,
    armed: bool,
}

impl HiddenJobCleanupGuard {
    fn new(clients: Arc<RunnerRegistry>, job_id: String, access: Option<RunnerAccess>) -> Self {
        Self {
            clients,
            job_id,
            access,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for HiddenJobCleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let clients = self.clients.clone();
        clients.record_hidden_cleanup_intent(self.job_id.clone(), self.access.clone());
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                clients.process_hidden_cleanup_intents().await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continued_execution_state_uses_the_fresh_observed_started_state() {
        assert_eq!(
            continued_execution_state("stop_requested", false),
            ("queued", false)
        );
        assert_eq!(
            continued_execution_state("stop_requested", true),
            ("running", true)
        );
        assert_eq!(
            continued_execution_state("agent_queued", false),
            ("queued", false)
        );
        assert_eq!(
            continued_execution_state("agent_queued", true),
            ("running", true)
        );
        assert_eq!(
            continued_execution_state("started", true),
            ("running", true)
        );
        assert_eq!(
            continued_execution_state("running", true),
            ("running", true)
        );
        assert_eq!(
            continued_execution_state("recovering", true),
            ("outcome_unknown", true)
        );
    }

    #[test]
    fn structured_execution_budget_preserves_default_and_validates_explicit_sync_wait() {
        let default = StructuredExecutionBudget::resolve_with_sync_wait(None, None).unwrap();
        assert_eq!(default.effective_timeout_secs, 60);
        assert_eq!(default.sync_wait_secs, 10);

        let short = StructuredExecutionBudget::resolve_with_sync_wait(Some(5), None).unwrap();
        assert_eq!(short.effective_timeout_secs, 5);
        assert_eq!(short.sync_wait_secs, 5);

        for wait in [1, 45, 60] {
            let budget =
                StructuredExecutionBudget::resolve_with_sync_wait(Some(600), Some(wait)).unwrap();
            assert_eq!(budget.effective_timeout_secs, 600);
            assert_eq!(budget.sync_wait_secs, wait);
        }

        assert!(StructuredExecutionBudget::resolve_with_sync_wait(Some(600), Some(0)).is_err());
        assert!(StructuredExecutionBudget::resolve_with_sync_wait(Some(600), Some(61)).is_err());
        let conflict = StructuredExecutionBudget::resolve_with_sync_wait(Some(5), Some(6))
            .expect_err("explicit sync wait above total timeout must be rejected");
        assert!(conflict.contains("must not exceed effective timeout_secs"));
    }
}
