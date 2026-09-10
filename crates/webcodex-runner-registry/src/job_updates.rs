use super::access_control::{
    assert_runner_access, job_visible_to_access as shell_job_visible_to_auth,
};
use super::jobs::{
    append_log_limited, assert_active_instance_locked, command_preview, is_final_job_status,
    job_view, notify_job_update, observe_job_terminal, parse_job_lifecycle, process_preview,
    refresh_job_status_locked, replace_log_limited, script_preview, select_log_lines,
};
use super::reconciliation::validate_stream_snapshot;
use super::requests::{
    enqueue_pending_request_locked, next_request_id, notify_runner_locked,
    remove_pending_request_locked,
};
use super::state::{
    DetachedIdempotencyIntent, JobLifecycleState, JobObservationState, JobRecoveryPhase,
    JobRecoveryReason, JobRecoveryState, ShellJobRecord, ShellJobVisibility,
};
use super::validation::{validate_id, validate_run_request, validate_runner_instance_id};
use super::{
    now_ts, RunnerFeature, RunnerRegistry, DETACHED_IDEMPOTENCY_CONFLICT,
    DETACHED_IDEMPOTENCY_RECOVERY_PREFIX,
};
use crate::DetachedInitiatorIdentity;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use uuid::Uuid;
use webcodex_core::runner_operation::{
    RunnerInvocationMetadata, RunnerJobOperation, RunnerJobProcessOperation,
    RunnerJobScriptOperation, RunnerJobShellOperation, RunnerJobValidationOperation,
    RunnerOperation,
};
use webcodex_core::runner_protocol::{
    validate_process_argv, validate_script_request, validation_infrastructure_failure_code,
    RunnerJobUpdateRequest, RunnerRequest, ShellCommandExecutionState, ShellJobActivity,
    ShellJobActivityPhase, ShellJobActivitySource, ShellJobContext, ShellJobInfo,
    ShellJobOpRequest, ShellJobStructuredExecutionMetadata, ShellJobValidationMetadata,
    ShellJobValidationStep, ShellProcessArgv, ShellRunRequest, ShellScriptLanguage,
    ShellScriptPayload, DETACHED_IDEMPOTENCY_KEY_MAX_BYTES, PROCESS_CWD_MAX_BYTES,
    PROCESS_STDIN_MAX_BYTES, STRUCTURED_EXECUTION_TIMEOUT_MAX_SECS,
    STRUCTURED_EXECUTION_TIMEOUT_MIN_SECS,
};

#[derive(Clone, Copy)]
struct ValidationProtocolError(&'static str);

/// Outcome of a bounded `job_log`/`job_tail` wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JobLogWaitOutcome {
    /// No wait was performed, or a returnable new state already existed at
    /// call time (a job advanced past `after_observation_token`).
    Immediate,
    /// A non-terminal update was observed after waiting.
    Updated,
    /// The job was (or became) terminal during the call.
    Terminal,
    /// The wait deadline elapsed with no observable change. This is a normal
    /// successful result, not a timeout failure.
    Timeout,
}

impl JobLogWaitOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::Updated => "updated",
            Self::Terminal => "terminal",
            Self::Timeout => "timeout",
        }
    }
}

/// Wait metadata returned by a bounded `job_log`/`job_tail` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobLogWait {
    pub wait_outcome: JobLogWaitOutcome,
    pub waited_ms: u64,
    /// Whether the job changed relative to the supplied `after_observation_token`.
    /// Always false when no `after_observation_token` was provided.
    pub changed: bool,
    /// Whether the job is terminal per the canonical job terminal definition.
    pub terminal: bool,
}

impl Default for JobLogWait {
    fn default() -> Self {
        Self {
            wait_outcome: JobLogWaitOutcome::Immediate,
            waited_ms: 0,
            changed: false,
            terminal: false,
        }
    }
}

/// Frozen Server-side observation details accompanying one public Job log
/// projection. The analysis context is bounded by the retained Server log and
/// is consumed only by validation/summary projection; it is never repeated as
/// model-facing output when the delta is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellJobLogObservation {
    pub wait: JobLogWait,
    pub log_delta_status: webcodex_core::job_observation::JobLogDeltaStatus,
    pub stdout_delta_reset: bool,
    pub stderr_delta_reset: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub stdout_returned_lines: usize,
    pub stderr_returned_lines: usize,
    pub analysis_stdout: String,
    pub analysis_stderr: String,
    pub analysis_truncated: bool,
}

impl std::ops::Deref for ShellJobLogObservation {
    type Target = JobLogWait;

    fn deref(&self) -> &Self::Target {
        &self.wait
    }
}

impl Default for ShellJobLogObservation {
    fn default() -> Self {
        Self {
            wait: JobLogWait::default(),
            log_delta_status: webcodex_core::job_observation::JobLogDeltaStatus::Baseline,
            stdout_delta_reset: false,
            stderr_delta_reset: false,
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_returned_lines: 0,
            stderr_returned_lines: 0,
            analysis_stdout: String::new(),
            analysis_stderr: String::new(),
            analysis_truncated: false,
        }
    }
}

fn frozen_shell_job_log_projection(
    job: &ShellJobRecord,
    after: Option<&webcodex_core::job_observation::JobObservationToken>,
    since_stdout_line: Option<usize>,
    since_stderr_line: Option<usize>,
    tail_lines: Option<usize>,
    wait: JobLogWait,
) -> (
    ShellJobInfo,
    Option<String>,
    Option<String>,
    usize,
    usize,
    ShellJobLogObservation,
) {
    let (analysis_stdout, _, _, analysis_stdout_truncated) =
        select_log_lines(&job.stdout, None, None);
    let (analysis_stderr, _, _, analysis_stderr_truncated) =
        select_log_lines(&job.stderr, None, None);
    let analysis_stdout = analysis_stdout.unwrap_or_default();
    let analysis_stderr = analysis_stderr.unwrap_or_default();
    let analysis_truncated = analysis_stdout_truncated
        || analysis_stderr_truncated
        || job.stdout.first_retained_line > 1
        || job.stderr.first_retained_line > 1;
    let explicit_paging = since_stdout_line.is_some() || since_stderr_line.is_some();

    if explicit_paging {
        // Preserve the established explicit cursor/tail precedence exactly.
        // The cursor-less token prevents a later automatic observation from
        // assuming that this explicit page represented complete log receipt.
        let (stdout, next_stdout_line, _, stdout_truncated) =
            select_log_lines(&job.stdout, since_stdout_line, tail_lines);
        let (stderr, next_stderr_line, _, stderr_truncated) =
            select_log_lines(&job.stderr, since_stderr_line, tail_lines);
        let mut view = job_view(job);
        view.observation_token = webcodex_core::job_observation::JobObservationToken::new_legacy(
            job.job_id.clone(),
            job.observation.epoch.to_string(),
            job.observation.revision.load(Ordering::Relaxed),
        )
        .ok()
        .map(|token| token.encode());
        let observation = ShellJobLogObservation {
            wait,
            log_delta_status: webcodex_core::job_observation::JobLogDeltaStatus::Baseline,
            stdout_delta_reset: false,
            stderr_delta_reset: false,
            stdout_truncated,
            stderr_truncated,
            stdout_returned_lines: stdout.as_deref().unwrap_or_default().lines().count(),
            stderr_returned_lines: stderr.as_deref().unwrap_or_default().lines().count(),
            analysis_stdout,
            analysis_stderr,
            analysis_truncated,
        };
        return (
            view,
            stdout,
            stderr,
            next_stdout_line,
            next_stderr_line,
            observation,
        );
    }

    let epoch_matches = after.is_none_or(|token| token.epoch == job.observation.epoch.as_ref());
    let base_mode = match after {
        None => webcodex_core::job_observation::JobLogSelectionMode::Baseline,
        Some(token) if token.is_legacy() || !epoch_matches => {
            webcodex_core::job_observation::JobLogSelectionMode::Reset
        }
        Some(token) => webcodex_core::job_observation::JobLogSelectionMode::Delta {
            cursor: token
                .stdout_cursor
                .expect("cursor-aware token has stdout cursor"),
        },
    };
    let stderr_mode = match after {
        None => webcodex_core::job_observation::JobLogSelectionMode::Baseline,
        Some(token) if token.is_legacy() || !epoch_matches => {
            webcodex_core::job_observation::JobLogSelectionMode::Reset
        }
        Some(token) => webcodex_core::job_observation::JobLogSelectionMode::Delta {
            cursor: token
                .stderr_cursor
                .expect("cursor-aware token has stderr cursor"),
        },
    };
    let stdout = webcodex_core::job_observation::project_log_stream(
        &job.stdout.tail,
        job.stdout.first_retained_line,
        job.stdout.next_line,
        job.stdout.truncated,
        tail_lines,
        base_mode,
        true,
    );
    let stderr = webcodex_core::job_observation::project_log_stream(
        &job.stderr.tail,
        job.stderr.first_retained_line,
        job.stderr.next_line,
        job.stderr.truncated,
        tail_lines,
        stderr_mode,
        true,
    );
    let mut view = job_view(job);
    view.observation_token = webcodex_core::job_observation::JobObservationToken::new(
        job.job_id.clone(),
        job.observation.epoch.to_string(),
        job.observation.revision.load(Ordering::Relaxed),
        stdout.next_line as u64,
        stderr.next_line as u64,
    )
    .ok()
    .map(|token| token.encode());
    let observation = ShellJobLogObservation {
        wait,
        log_delta_status: webcodex_core::job_observation::combined_delta_status(
            base_mode, &stdout, &stderr,
        ),
        stdout_delta_reset: stdout.delta_reset,
        stderr_delta_reset: stderr.delta_reset,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        stdout_returned_lines: stdout.returned_lines,
        stderr_returned_lines: stderr.returned_lines,
        analysis_stdout,
        analysis_stderr,
        analysis_truncated,
    };
    (
        view,
        Some(stdout.text),
        Some(stderr.text),
        stdout.next_line,
        stderr.next_line,
        observation,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JobPublicMutationSignature {
    lifecycle: JobLifecycleState,
    recovery: JobRecoveryState,
    last_update_seq: u64,
    stdout: super::state::ShellJobLogState,
    stderr: super::state::ShellJobLogState,
    error: Option<String>,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    exit_code: Option<i32>,
    duration_ms: Option<u64>,
    command_execution_state: Option<ShellCommandExecutionState>,
    validation_progress: Option<webcodex_core::runner_protocol::ShellJobValidationProgress>,
    activity: Option<ShellJobActivity>,
    recovered_after_server_restart: bool,
    reconciled_at: Option<i64>,
}

fn public_mutation_signature(job: &ShellJobRecord) -> JobPublicMutationSignature {
    JobPublicMutationSignature {
        lifecycle: job.lifecycle,
        recovery: job.recovery.clone(),
        last_update_seq: job.last_update_seq,
        stdout: job.stdout.clone(),
        stderr: job.stderr.clone(),
        error: job.error.clone(),
        started_at: job.started_at,
        ended_at: job.ended_at,
        exit_code: job.exit_code,
        duration_ms: job.duration_ms,
        command_execution_state: job.command_execution_state,
        validation_progress: job.validation_progress.clone(),
        activity: job.activity,
        recovered_after_server_restart: job.recovery.recovered_after_server_restart,
        reconciled_at: job.recovery.reconciled_at,
    }
}

fn invalid_progress(code: &'static str) -> Result<(), ValidationProtocolError> {
    Err(ValidationProtocolError(code))
}

fn validate_validation_progress(
    job: &ShellJobRecord,
    update: &RunnerJobUpdateRequest,
    lifecycle: JobLifecycleState,
) -> Result<(), ValidationProtocolError> {
    if job.validation_steps.is_empty() {
        return if update.validation_progress.is_none() {
            Ok(())
        } else {
            invalid_progress("validation_progress_unexpected")
        };
    }
    if job.validation_steps.iter().collect::<HashSet<_>>().len() != job.validation_steps.len() {
        return invalid_progress("validation_plan_invalid");
    }
    if update.finished && !lifecycle.is_terminal() {
        return invalid_progress("validation_progress_invalid");
    }
    let cancelling = matches!(
        lifecycle,
        JobLifecycleState::Stopped
            | JobLifecycleState::Cancelled
            | JobLifecycleState::Timeout
            | JobLifecycleState::TimedOut
            | JobLifecycleState::Lost
    );
    let Some(progress) = update.validation_progress.as_ref() else {
        if matches!(
            lifecycle,
            JobLifecycleState::Queued | JobLifecycleState::RunnerQueued
        ) || cancelling
        {
            return Ok(());
        }
        return invalid_progress("validation_progress_missing");
    };
    if matches!(
        lifecycle,
        JobLifecycleState::Queued | JobLifecycleState::RunnerQueued
    ) || progress.completed > job.validation_steps.len()
    {
        return invalid_progress("validation_progress_invalid");
    }
    let previous = job
        .validation_progress
        .as_ref()
        .map(|progress| progress.completed)
        .unwrap_or(0);
    // The `completed` cursor advances at most one step per update: an equal
    // value is an idempotent replay, `previous + 1` is the legitimate single
    // advance, and anything else is a protocol violation. Skipping ahead past
    // unreported steps (or regressing) corrupts the fail-fast validation plan,
    // so reject both directions rather than only the regression.
    if progress.completed < previous || progress.completed > previous.saturating_add(1) {
        return invalid_progress("validation_progress_invalid");
    }
    let no_active_step = progress.current_step.is_none() && progress.failed_step.is_none();
    let infrastructure_failure = lifecycle == JobLifecycleState::Failed
        && update.finished
        && update.exit_code.is_none()
        && update
            .error
            .as_deref()
            .and_then(validation_infrastructure_failure_code)
            .is_some();
    let valid = if cancelling {
        no_active_step
    } else if infrastructure_failure {
        progress.completed < job.validation_steps.len() && no_active_step
    } else if !lifecycle.is_terminal() {
        let expected = job.validation_steps.get(progress.completed);
        expected.map(String::as_str) == progress.current_step.as_deref()
            && progress.failed_step.is_none()
    } else if lifecycle == JobLifecycleState::Completed && update.exit_code == Some(0) {
        progress.completed == job.validation_steps.len() && no_active_step
    } else if lifecycle == JobLifecycleState::Failed {
        let expected = job.validation_steps.get(progress.completed);
        expected.map(String::as_str) == progress.failed_step.as_deref()
            && progress.current_step.is_none()
    } else {
        false
    };
    if valid {
        Ok(())
    } else if lifecycle == JobLifecycleState::Completed && update.exit_code == Some(0) {
        invalid_progress("validation_progress_incomplete")
    } else {
        invalid_progress("validation_progress_invalid")
    }
}

fn validation_activity_phase(step: &str) -> Option<ShellJobActivityPhase> {
    match step {
        "format" => Some(ShellJobActivityPhase::ValidationFormat),
        "check" => Some(ShellJobActivityPhase::ValidationCheck),
        "test" => Some(ShellJobActivityPhase::ValidationTest),
        _ => None,
    }
}

fn validate_job_activity(
    job: &ShellJobRecord,
    update: &RunnerJobUpdateRequest,
    lifecycle: JobLifecycleState,
) -> Result<(), ValidationProtocolError> {
    let Some(activity) = update.activity else {
        // Activity was added as an optional protocol field. Older Runners may
        // omit it without changing canonical Job lifecycle semantics.
        return Ok(());
    };
    if !activity.is_canonical()
        || !matches!(
            lifecycle,
            JobLifecycleState::Running | JobLifecycleState::StopRequested
        )
    {
        return invalid_progress("job_activity_invalid");
    }
    match activity.source {
        ShellJobActivitySource::RunnerExecution => {
            if !job.validation_steps.is_empty()
                || activity.phase != ShellJobActivityPhase::ProcessRunning
            {
                return invalid_progress("job_activity_invalid");
            }
        }
        ShellJobActivitySource::ValidationPlan => {
            let Some(current_step) = update
                .validation_progress
                .as_ref()
                .and_then(|progress| progress.current_step.as_deref())
            else {
                return invalid_progress("job_activity_invalid");
            };
            if validation_activity_phase(current_step) != Some(activity.phase) {
                return invalid_progress("job_activity_invalid");
            }
        }
        ShellJobActivitySource::CargoOutput => {
            let Some(progress) = update.validation_progress.as_ref() else {
                return invalid_progress("job_activity_invalid");
            };
            if progress.current_step.is_none() || job.validation_steps.is_empty() {
                return invalid_progress("job_activity_invalid");
            }
            // First-class validation metadata retains exact canonical argv, so
            // verify Cargo provenance when that stronger evidence is available.
            // Multi-step checks_run plans retain only step names server-side;
            // there the trusted Runner remains the bounded provenance boundary.
            if let Some(validation) = job.validation.as_ref() {
                if validation
                    .steps
                    .get(progress.completed)
                    .is_none_or(|step| step.program != "cargo" || !step.is_canonical())
                {
                    return invalid_progress("job_activity_invalid");
                }
            }
        }
    }
    Ok(())
}

fn validate_command_execution_state(
    job: &ShellJobRecord,
    update: &RunnerJobUpdateRequest,
    lifecycle: JobLifecycleState,
) -> Result<(), ValidationProtocolError> {
    let terminal = lifecycle.is_terminal();
    if !terminal && update.command_execution_state.is_some() {
        return invalid_progress("command_execution_state_on_active_job");
    }
    if job.structured_execution.is_some() && terminal && update.command_execution_state.is_none() {
        return invalid_progress("structured_job_lifecycle_missing");
    }
    let Some(state) = update.command_execution_state else {
        return Ok(());
    };
    let valid = match state {
        ShellCommandExecutionState::NotStarted => {
            matches!(
                lifecycle,
                JobLifecycleState::Failed
                    | JobLifecycleState::Stopped
                    | JobLifecycleState::Cancelled
                    | JobLifecycleState::Lost
            ) && job.started_at.is_none()
        }
        ShellCommandExecutionState::OutcomeUnknown => matches!(
            lifecycle,
            JobLifecycleState::Failed | JobLifecycleState::Lost
        ),
        ShellCommandExecutionState::TimedOut => lifecycle.is_timed_out(),
        ShellCommandExecutionState::Completed => matches!(
            lifecycle,
            JobLifecycleState::Completed
                | JobLifecycleState::Failed
                | JobLifecycleState::Stopped
                | JobLifecycleState::Cancelled
        ),
    };
    if valid {
        Ok(())
    } else {
        invalid_progress("structured_job_lifecycle_invalid")
    }
}

#[derive(Debug, Clone, Default)]
pub struct ShellJobStartMetadata {
    pub project_id: Option<String>,
    pub session_id: Option<String>,
    pub ssh_resource: Option<String>,
    pub project_cwd: Option<String>,
    pub purpose: Option<String>,
    pub shell: Option<String>,
    pub validation_steps: Vec<ShellJobValidationStep>,
    pub validation: Option<ShellJobValidationMetadata>,
    pub visibility: ShellJobVisibility,
    pub validation_identity: Option<String>,
    pub validation_tool: Option<String>,
    pub assertion_name: Option<String>,
    pub structured_execution: Option<StructuredJobExecution>,
    pub stdin: Option<String>,
    /// Detached-only caller replay key. Consumed to derive the logical Job
    /// identity; never copied into Runner protocol, durable state, or audit.
    pub detached_idempotency_key: Option<String>,
}

#[derive(Debug, Clone)]
pub enum StructuredJobExecution {
    Process(ShellProcessArgv),
    DetachedProcess(ShellProcessArgv),
    Script(ShellScriptPayload),
}

fn validate_structured_job_common(
    cwd: Option<&str>,
    stdin: Option<&str>,
    timeout_secs: u64,
) -> Result<(), String> {
    if let Some(stdin) = stdin {
        if stdin.len() > PROCESS_STDIN_MAX_BYTES {
            return Err(format!(
                "stdin is too large; maximum is {PROCESS_STDIN_MAX_BYTES} bytes"
            ));
        }
        if stdin.contains('\0') {
            return Err("stdin cannot contain NUL bytes".to_string());
        }
    }
    if let Some(cwd) = cwd {
        if cwd.len() > PROCESS_CWD_MAX_BYTES {
            return Err(format!(
                "cwd is too long; maximum is {PROCESS_CWD_MAX_BYTES} bytes"
            ));
        }
        if cwd.contains('\0') {
            return Err("cwd cannot contain NUL bytes".to_string());
        }
    }
    if !(STRUCTURED_EXECUTION_TIMEOUT_MIN_SECS..=STRUCTURED_EXECUTION_TIMEOUT_MAX_SECS)
        .contains(&timeout_secs)
    {
        return Err(format!(
            "timeout_secs must be between {STRUCTURED_EXECUTION_TIMEOUT_MIN_SECS} and {STRUCTURED_EXECUTION_TIMEOUT_MAX_SECS}"
        ));
    }
    Ok(())
}

fn validate_detached_idempotency_key(key: &str) -> Result<(), String> {
    if key.is_empty()
        || key.len() > DETACHED_IDEMPOTENCY_KEY_MAX_BYTES
        || key.contains(['\0', '\r', '\n'])
    {
        return Err(format!(
            "detached idempotency_key must be 1..={DETACHED_IDEMPOTENCY_KEY_MAX_BYTES} bytes and contain no NUL/CR/LF"
        ));
    }
    Ok(())
}

fn detached_job_id_for_key(
    initiator: &DetachedInitiatorIdentity,
    key: &str,
) -> Result<String, String> {
    validate_detached_idempotency_key(key)?;
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex-detached-initiation-v1\0");
    hasher.update(initiator.as_stable_principal().as_bytes());
    hasher.update(b"\0");
    hasher.update(key.as_bytes());
    Ok(format!("detached_{:x}", hasher.finalize()))
}

impl RunnerRegistry {
    pub async fn start_job(
        &self,
        body: ShellJobOpRequest,
        requested_by: String,
    ) -> Result<ShellJobInfo, String> {
        self.start_job_with_metadata(body, requested_by, ShellJobStartMetadata::default())
            .await
    }

    pub async fn start_job_with_metadata(
        &self,
        body: ShellJobOpRequest,
        requested_by: String,
        metadata: ShellJobStartMetadata,
    ) -> Result<ShellJobInfo, String> {
        let detached_initiator = metadata
            .detached_idempotency_key
            .as_ref()
            .map(|_| DetachedInitiatorIdentity::internal());
        self.start_job_with_metadata_for_access(
            body,
            requested_by,
            metadata,
            None,
            detached_initiator.as_ref(),
        )
        .await
    }

    pub async fn start_job_with_metadata_for_access(
        &self,
        body: ShellJobOpRequest,
        requested_by: String,
        metadata: ShellJobStartMetadata,
        access: Option<&crate::RunnerAccess>,
        detached_initiator: Option<&DetachedInitiatorIdentity>,
    ) -> Result<ShellJobInfo, String> {
        let client_id = body
            .client_id
            .clone()
            .ok_or_else(|| "client_id is required for op=start".to_string())?;
        let command = body
            .command
            .clone()
            .ok_or_else(|| "command is required for op=start".to_string())?;
        let timeout_secs = body.timeout_secs.unwrap_or(120);
        let normalized_job_cwd = body.cwd.clone().map(|cwd| cwd.trim().to_string());
        let request_id = next_request_id();
        let created_at = now_ts();
        // Ordinary Session-scoped jobs retain their Session id even without an
        // SSH resource. Only the inverse is invalid: remote execution without
        // the Workflow Session that owns the SSH context.
        if metadata.ssh_resource.is_some() && metadata.session_id.is_none() {
            return Err(
                "ssh_session_required: an SSH resource requires a Workflow Session id".to_string(),
            );
        }
        if metadata.ssh_resource.as_deref().is_some_and(|resource| {
            resource.is_empty()
                || resource.len() > 80
                || resource.contains("..")
                || !resource.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
                })
        }) {
            return Err("ssh_resource_invalid: resource name is invalid".to_string());
        }
        let validation_steps = metadata.validation_steps;
        let validation = metadata.validation;
        let validation_identity = metadata.validation_identity.clone();
        let validation_tool = metadata.validation_tool.clone();
        let assertion_name = metadata.assertion_name.clone();
        let structured_execution = metadata.structured_execution;
        let javascript_script_request = matches!(
            structured_execution.as_ref(),
            Some(StructuredJobExecution::Script(script))
                if script.language == ShellScriptLanguage::Javascript
        );
        let structured_stdin = metadata.stdin;
        if validation_steps.len() > 3
            || validation_steps.iter().any(|step| !step.is_canonical())
            || validation_steps
                .iter()
                .map(|step| step.name.as_str())
                .collect::<HashSet<_>>()
                .len()
                != validation_steps.len()
        {
            return Err("invalid structured validation plan".to_string());
        }
        if validation
            .as_ref()
            .is_some_and(|metadata| !metadata.is_valid() || metadata.steps != validation_steps)
        {
            return Err("invalid structured validation metadata".to_string());
        }
        if structured_execution.is_some()
            && (!validation_steps.is_empty() || validation.is_some() || !command.is_empty())
        {
            return Err(
                "typed structured Job starts require command=\"\" and no validation plan"
                    .to_string(),
            );
        }
        let (safe_command_preview, structured_metadata, job_kind) = match structured_execution
            .as_ref()
        {
            Some(StructuredJobExecution::Process(process)) => {
                validate_process_argv(process)?;
                validate_structured_job_common(
                    normalized_job_cwd.as_deref(),
                    structured_stdin.as_deref(),
                    timeout_secs,
                )?;
                let preview =
                    process_preview(&process.executable, process.args.iter().map(String::as_str));
                let safe = ShellJobStructuredExecutionMetadata {
                    execution_source: "run_process".to_string(),
                    language: None,
                    script_bytes: None,
                    arg_count: process.args.len(),
                    stdin_present: structured_stdin.is_some(),
                    validation_identity: validation_identity.clone(),
                    validation_tool: validation_tool.clone(),
                    assertion_name: assertion_name.clone(),
                };
                (preview, Some(safe), "run_process")
            }
            Some(StructuredJobExecution::DetachedProcess(process)) => {
                validate_process_argv(process)?;
                validate_structured_job_common(
                    normalized_job_cwd.as_deref(),
                    structured_stdin.as_deref(),
                    timeout_secs,
                )?;
                let preview = format!("detached process ({} args)", process.args.len());
                let safe = ShellJobStructuredExecutionMetadata {
                    execution_source: "run_detached_process".to_string(),
                    language: None,
                    script_bytes: None,
                    arg_count: process.args.len(),
                    stdin_present: structured_stdin.is_some(),
                    validation_identity: validation_identity.clone(),
                    validation_tool: validation_tool.clone(),
                    assertion_name: None,
                };
                (preview, Some(safe), "run_detached_process")
            }
            Some(StructuredJobExecution::Script(script)) => {
                validate_script_request(
                    script,
                    structured_stdin.as_deref(),
                    normalized_job_cwd.as_deref(),
                    timeout_secs,
                )?;
                let preview = script_preview(
                    script.language.as_str(),
                    script.script.len(),
                    script.args.len(),
                );
                let safe = ShellJobStructuredExecutionMetadata {
                    execution_source: "run_script".to_string(),
                    language: Some(script.language),
                    script_bytes: Some(script.script.len()),
                    arg_count: script.args.len(),
                    stdin_present: structured_stdin.is_some(),
                    validation_identity: validation_identity.clone(),
                    validation_tool: validation_tool.clone(),
                    assertion_name: assertion_name.clone(),
                };
                (preview, Some(safe), "run_script")
            }
            None => {
                let run = ShellRunRequest {
                    client_id: client_id.clone(),
                    cwd: normalized_job_cwd.clone(),
                    command: command.clone(),
                    stdin: None,
                    timeout_secs,
                    wait_timeout_secs: 0,
                };
                validate_run_request(&run)?;
                let preview = if validation_steps.is_empty() {
                    command_preview(&run.command)
                } else {
                    format!(
                        "validation: {}",
                        validation_steps
                            .iter()
                            .map(|step| step.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                (preview, None, "shell")
            }
        };
        let detached_request = matches!(
            structured_execution.as_ref(),
            Some(StructuredJobExecution::DetachedProcess(_))
        );
        let detached_idempotency_key = metadata.detached_idempotency_key.as_deref();
        if detached_request != detached_idempotency_key.is_some() {
            return Err(
                "detached idempotency_key must be present exactly for detached process Job starts"
                    .to_string(),
            );
        }
        let detached_intent = if let Some(StructuredJobExecution::DetachedProcess(process)) =
            structured_execution.as_ref()
        {
            Some(DetachedIdempotencyIntent {
                project_id: metadata.project_id.clone(),
                session_id: metadata.session_id.clone(),
                project_cwd: metadata.project_cwd.clone(),
                cwd: normalized_job_cwd.clone(),
                purpose: metadata.purpose.clone(),
                shell: metadata.shell.clone(),
                process: process.clone(),
                stdin: structured_stdin.clone(),
                timeout_secs,
            })
        } else {
            None
        };
        let job_id = match detached_idempotency_key {
            Some(key) => detached_job_id_for_key(
                detached_initiator.ok_or_else(|| {
                    "detached idempotency requires a stable authenticated caller identity"
                        .to_string()
                })?,
                key,
            )?,
            None => Uuid::new_v4().to_string(),
        };
        let validation_step_names = validation_steps
            .iter()
            .map(|step| step.name.clone())
            .collect::<Vec<_>>();
        let job_context = ShellJobContext {
            runtime_project_id: metadata.project_id.clone(),
            workflow_session_id: metadata.session_id.clone(),
            ssh_resource: metadata.ssh_resource.clone(),
            project_cwd: metadata.project_cwd.clone(),
            cwd: normalized_job_cwd.clone(),
            purpose: metadata.purpose.clone(),
            shell: metadata.shell.clone(),
            command_preview: safe_command_preview.clone(),
            validation_steps: validation_step_names.clone(),
            validation: validation.clone(),
            structured_execution: structured_metadata.clone(),
        };
        let job_operation = match structured_execution {
            Some(StructuredJobExecution::Process(process)) => {
                RunnerJobOperation::StartProcess(RunnerJobProcessOperation {
                    job_id: job_id.clone(),
                    cwd: normalized_job_cwd.clone(),
                    process,
                    stdin: structured_stdin.clone(),
                    timeout_secs,
                    context: job_context,
                })
            }
            Some(StructuredJobExecution::DetachedProcess(process)) => {
                RunnerJobOperation::StartDetachedProcess(RunnerJobProcessOperation {
                    job_id: job_id.clone(),
                    cwd: normalized_job_cwd.clone(),
                    process,
                    stdin: structured_stdin.clone(),
                    timeout_secs,
                    context: job_context,
                })
            }
            Some(StructuredJobExecution::Script(script)) => {
                RunnerJobOperation::StartScript(RunnerJobScriptOperation {
                    job_id: job_id.clone(),
                    cwd: normalized_job_cwd.clone(),
                    script,
                    stdin: structured_stdin.clone(),
                    timeout_secs,
                    context: job_context,
                })
            }
            None if validation_steps.is_empty() => {
                RunnerJobOperation::StartShell(RunnerJobShellOperation {
                    job_id: job_id.clone(),
                    cwd: normalized_job_cwd.clone(),
                    command: command.clone(),
                    timeout_secs,
                    context: job_context,
                })
            }
            None => RunnerJobOperation::StartValidation(RunnerJobValidationOperation {
                job_id: job_id.clone(),
                cwd: normalized_job_cwd.clone(),
                steps: validation_steps.clone(),
                timeout_secs,
                context: job_context,
            }),
        };
        debug_assert_eq!(job_operation.is_detached_process(), detached_request);
        let request = RunnerRequest::from_operation(
            RunnerInvocationMetadata {
                request_id: request_id.clone(),
                client_id: client_id.clone(),
                requested_by,
                created_at,
            },
            RunnerOperation::Job(job_operation),
        )?;
        let mut inner = self.inner.lock().await;
        let Some(runner) = inner.runners.get(&client_id) else {
            return Err(format!("unknown shell client: {}", client_id));
        };
        if access.is_some() {
            assert_runner_access(access, runner)?;
        }
        if !(runner.runner_features.supports(RunnerFeature::AsyncJobs)
            || runner
                .runner_features
                .supports(RunnerFeature::AsyncShellJobs))
        {
            return Err(format!(
                "runner {} does not support async shell jobs",
                client_id
            ));
        }
        if structured_metadata.is_some()
            && !runner
                .runner_features
                .supports(RunnerFeature::StructuredExecutionJobs)
        {
            return Err(format!(
                "capability_unavailable: runner {client_id} does not support structured_execution_jobs"
            ));
        }
        if javascript_script_request
            && !runner
                .runner_features
                .supports(RunnerFeature::StructuredScriptJavascript)
        {
            return Err(format!(
                "capability_unavailable: runner {client_id} does not support structured_script_javascript"
            ));
        }
        if detached_request
            && !runner
                .runner_features
                .supports(RunnerFeature::DetachedProcessJobs)
        {
            return Err(format!(
                "capability_unavailable: runner {client_id} does not support detached_process_jobs"
            ));
        }
        if structured_metadata.is_some() && metadata.ssh_resource.is_some() {
            return Err(
                "ssh_resource_unsupported_for_request: typed structured Jobs do not support SSH resources"
                    .to_string(),
            );
        }
        if metadata.ssh_resource.is_some()
            && !runner.runner_features.supports(RunnerFeature::SshShell)
        {
            return Err(format!(
                "agent_capability_unavailable: runner {} does not support ssh_shell",
                client_id
            ));
        }
        if !validation_steps.is_empty()
            && !runner
                .runner_features
                .supports(RunnerFeature::StructuredValidationArgv)
        {
            return Err(format!(
                "structured_validation_unavailable: runner {} does not support structured argv validation jobs",
                client_id
            ));
        }
        if validation
            .as_ref()
            .and_then(|metadata| metadata.minimum_tests)
            .is_some()
            && !runner
                .runner_features
                .supports(RunnerFeature::StructuredCargoTestCountAssertion)
        {
            return Err(format!(
                "structured_cargo_test_count_assertion_unavailable: runner {} does not support durable Cargo test-count assertions",
                client_id
            ));
        }
        if validation
            .as_ref()
            .is_some_and(|metadata| metadata.require_tests.is_some() || metadata.no_run.is_some())
            && !runner
                .runner_features
                .supports(RunnerFeature::StructuredCargoTestExecutionPolicy)
        {
            return Err(format!(
                "structured_cargo_test_execution_policy_unavailable: runner {} does not support durable Cargo validation execution-policy metadata",
                client_id
            ));
        }
        if validation_steps
            .iter()
            .any(webcodex_core::runner_protocol::ShellJobValidationStep::is_structured_go_test_json)
            && !runner
                .runner_features
                .supports(RunnerFeature::StructuredGoTestJson)
        {
            return Err(format!(
                "structured_go_test_json_unavailable: runner {} does not support machine-readable Go test validation",
                client_id
            ));
        }
        if validation_steps.iter().any(|step| {
            step.is_structured_go_test_json() && step.args.as_slice() != ["test", "-json", "./..."]
        }) && !runner
            .runner_features
            .supports(RunnerFeature::StructuredGoTestPackages)
        {
            return Err(format!(
                "structured_go_test_packages_unavailable: runner {} does not support focused Go package validation argv",
                client_id
            ));
        }
        if validation
            .as_ref()
            .is_some_and(|metadata| metadata.tool == "go_test")
            && !runner
                .runner_features
                .supports(RunnerFeature::StructuredGoTestTool)
        {
            return Err(format!(
                "structured_go_test_tool_unavailable: runner {} does not support the first-class go_test validation contract",
                client_id
            ));
        }
        if metadata
            .project_id
            .as_deref()
            .is_some_and(|project| inner.unregistering_projects.contains_key(project))
        {
            return Err("project_unregister_in_progress".to_string());
        }
        let runner_instance_id = runner.runner_instance_id.clone();
        let auth_group = runner.auth_group.clone();
        if let Some(intent) = detached_intent.as_ref() {
            if let Some(existing) = inner.jobs_by_id.get(&job_id) {
                if existing.kind != "run_detached_process" {
                    return Err(format!(
                        "{DETACHED_IDEMPOTENCY_CONFLICT}: idempotency key resolves to a non-detached Job"
                    ));
                }
                match existing.detached_idempotency_intent.as_ref() {
                    Some(existing_intent) if existing_intent == intent => {
                        return Ok(job_view(existing));
                    }
                    Some(_) => {
                        return Err(format!(
                            "{DETACHED_IDEMPOTENCY_CONFLICT}: idempotency key was already used for different detached process intent"
                        ));
                    }
                    None => {
                        return Err(format!("{DETACHED_IDEMPOTENCY_RECOVERY_PREFIX}{job_id}"));
                    }
                }
            }
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            &client_id,
            request_id.clone(),
            request,
            None,
            Some(job_id.clone()),
        )?;
        let job = ShellJobRecord {
            job_id: job_id.clone(),
            request_id: Some(request_id.clone()),
            client_id: client_id.clone(),
            auth_group,
            runner_instance_id,
            kind: job_kind.to_string(),
            project_id: metadata.project_id,
            session_id: metadata.session_id,
            ssh_resource: metadata.ssh_resource,
            cwd: normalized_job_cwd,
            project_cwd: metadata.project_cwd,
            purpose: metadata.purpose,
            shell: metadata.shell,
            command_preview: safe_command_preview,
            detached_idempotency_intent: detached_intent,
            lifecycle: JobLifecycleState::Queued,
            created_at,
            started_at: None,
            ended_at: None,
            exit_code: None,
            duration_ms: None,
            stdout: Default::default(),
            stderr: Default::default(),
            error: None,
            command_execution_state: None,
            structured_execution: structured_metadata,
            codex: body.codex.clone(),
            validation_steps: validation_step_names,
            validation,
            validation_progress: None,
            activity: None,
            last_update_seq: 0,
            visibility: metadata.visibility,

            recovery: JobRecoveryState::default(),
            observation: JobObservationState::new(self.observation_epoch.clone()),
        };
        inner.request_to_job.insert(request_id, job_id.clone());
        inner.jobs_by_id.insert(job_id.clone(), job);
        notify_runner_locked(&inner, &client_id);
        Ok(job_view(
            inner.jobs_by_id.get(&job_id).expect("job just inserted"),
        ))
    }

    #[cfg(any(test, feature = "root-test-support"))]
    pub async fn hidden_job_ids_for_test(&self) -> Vec<String> {
        let inner = self.inner.lock().await;
        let mut ids = inner
            .jobs_by_id
            .values()
            .filter(|job| job.visibility != ShellJobVisibility::Public)
            .map(|job| job.job_id.clone())
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    pub async fn promote_hidden_job(&self, job_id: &str) -> Result<ShellJobInfo, String> {
        let mut inner = self.inner.lock().await;
        refresh_job_status_locked(&mut inner, job_id);
        let job = inner
            .jobs_by_id
            .get_mut(job_id)
            .ok_or_else(|| format!("unknown shell job: {job_id}"))?;
        if job.visibility == ShellJobVisibility::CleanupPending {
            return Err(format!("structured job cleanup is pending: {job_id}"));
        }
        // A terminal update may race the sync-wait deadline. Keep terminal
        // records hidden so the initiating structured tool call returns its
        // terminal result instead of handing off an already-finished Job.
        if !job.lifecycle.is_terminal() {
            let view = job_view(job);
            if view.observation_token.is_none() {
                return Err(format!(
                    "structured job has no canonical observation token: {job_id}"
                ));
            }
            job.visibility = ShellJobVisibility::Public;
            return Ok(view);
        }
        Ok(job_view(job))
    }

    pub async fn get_hidden_job_for_auth(
        &self,
        auth: Option<&crate::RunnerAccess>,
        job_id: &str,
    ) -> Result<ShellJobInfo, String> {
        validate_id(job_id, "job_id")?;
        let mut inner = self.inner.lock().await;
        refresh_job_status_locked(&mut inner, job_id);
        let job = inner
            .jobs_by_id
            .get(job_id)
            .ok_or_else(|| format!("unknown shell job: {job_id}"))?;
        if !shell_job_visible_to_auth(auth, &inner, job) {
            return Err(format!("unknown shell job: {job_id}"));
        }
        Ok(job_view(job))
    }

    pub async fn hidden_job_log_for_auth(
        &self,
        auth: Option<&crate::RunnerAccess>,
        job_id: &str,
        tail_lines: Option<usize>,
    ) -> Result<(ShellJobInfo, Option<String>, Option<String>, usize, usize), String> {
        validate_id(job_id, "job_id")?;
        let mut inner = self.inner.lock().await;
        refresh_job_status_locked(&mut inner, job_id);
        let job = inner
            .jobs_by_id
            .get(job_id)
            .ok_or_else(|| format!("unknown shell job: {job_id}"))?;
        if !shell_job_visible_to_auth(auth, &inner, job) {
            return Err(format!("unknown shell job: {job_id}"));
        }
        let (stdout, next_stdout_line, _, _) = select_log_lines(&job.stdout, None, tail_lines);
        let (stderr, next_stderr_line, _, _) = select_log_lines(&job.stderr, None, tail_lines);
        Ok((
            job_view(job),
            stdout,
            stderr,
            next_stdout_line,
            next_stderr_line,
        ))
    }

    /// Record hidden structured-execution cleanup synchronously. This is safe
    /// to call from a future's Drop implementation and is deliberately
    /// separate from stop delivery: the periodic registry lifecycle retries
    /// any intent whose immediate asynchronous processor is delayed.
    pub fn record_hidden_cleanup_intent(&self, job_id: String, auth: Option<crate::RunnerAccess>) {
        self.cleanup_intents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(job_id, auth);
    }

    pub async fn process_hidden_cleanup_intents(&self) {
        let intents = {
            let mut intents = self
                .cleanup_intents
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *intents)
        };
        let mut retry = Vec::new();
        for (job_id, auth) in intents {
            match self
                .cancel_hidden_job_for_auth(auth.as_ref(), &job_id)
                .await
            {
                Ok(_) => {}
                Err(error) if error.starts_with("unknown shell job:") => {}
                Err(_) => retry.push((job_id, auth)),
            }
        }
        if !retry.is_empty() {
            let mut intents = self
                .cleanup_intents
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            intents.extend(retry);
        }
    }

    #[cfg(any(test, feature = "root-test-support"))]
    pub fn has_hidden_cleanup_intent_for_test(&self, job_id: &str) -> bool {
        self.cleanup_intents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(job_id)
    }

    pub(crate) async fn cancel_hidden_job_for_auth(
        &self,
        auth: Option<&crate::RunnerAccess>,
        job_id: &str,
    ) -> Result<bool, String> {
        validate_id(job_id, "job_id")?;
        let mut inner = self.inner.lock().await;
        refresh_job_status_locked(&mut inner, job_id);
        let job = inner
            .jobs_by_id
            .get(job_id)
            .cloned()
            .ok_or_else(|| format!("unknown shell job: {job_id}"))?;
        if job.visibility == ShellJobVisibility::Public
            || !shell_job_visible_to_auth(auth, &inner, &job)
        {
            return Err(format!("unknown shell job: {job_id}"));
        }
        if job.lifecycle == JobLifecycleState::Queued {
            if let Some(request_id) = job.request_id.as_deref() {
                remove_pending_request_locked(&mut inner, request_id);
                inner.request_to_job.remove(request_id);
            }
            inner.jobs_by_id.remove(job_id);
            return Ok(true);
        }
        if job.lifecycle.is_terminal() {
            inner.jobs_by_id.remove(job_id);
            return Ok(true);
        }
        inner
            .jobs_by_id
            .get_mut(job_id)
            .expect("job exists")
            .visibility = ShellJobVisibility::CleanupPending;
        if !job.recovery_active()
            && job.lifecycle.is_runner_active()
            && job.lifecycle != JobLifecycleState::StopRequested
        {
            let stop_request_id = next_request_id();
            let request = RunnerRequest::from_operation(
                RunnerInvocationMetadata {
                    request_id: stop_request_id.clone(),
                    client_id: job.client_id.clone(),
                    requested_by: "tool_runtime_cleanup".to_string(),
                    created_at: now_ts(),
                },
                RunnerOperation::Job(RunnerJobOperation::Stop {
                    job_id: job_id.to_string(),
                }),
            )?;
            enqueue_pending_request_locked(
                self.telemetry.as_ref(),
                &mut inner,
                &job.client_id,
                stop_request_id,
                request,
                None,
                Some(job_id.to_string()),
            )?;
            let record = inner.jobs_by_id.get_mut(job_id).expect("job exists");
            record.lifecycle = JobLifecycleState::StopRequested;
            record.error = Some("internal structured execution cleanup requested".to_string());
            notify_runner_locked(&inner, &job.client_id);
        }
        Ok(false)
    }

    /// Remove a Runner-backed Job record entirely.
    /// The caller must ensure the job is terminal or stopped first; removing a
    /// still-active record would orphan the runner process (its later updates
    /// then fail harmlessly as "unknown shell job"). Also drops any still
    /// pending start request so a queued-but-never-run job leaves no request
    /// behind.
    #[cfg(any(test, feature = "root-test-support"))]
    pub async fn remove_job_record(&self, job_id: &str) -> bool {
        let mut inner = self.inner.lock().await;
        let Some(job) = inner.jobs_by_id.remove(job_id) else {
            return false;
        };
        if let Some(request_id) = job.request_id {
            inner.request_to_job.remove(&request_id);
            inner.pending_by_id.remove(&request_id);
            if let Some(queue) = inner.queues_by_runner.get_mut(&job.client_id) {
                queue.retain(|id| id != &request_id);
            }
        }
        true
    }

    /// Discard a terminal hidden Job after its result has already been projected
    /// into the initiating tool response.
    ///
    /// The exact discarded handle is retained only in the current Server
    /// process, bounded by the Runner terminal-inventory limits. A same-runner
    /// registration replay can then suppress that already-projected terminal
    /// evidence without changing fresh-Server conservative recovery. This is
    /// shared by typed process/script execution, structured validation, and
    /// long `run_shell`; all of them use `HiddenUntilHandoff` before projection.
    pub async fn remove_projected_hidden_terminal_job_record(&self, job_id: &str) -> bool {
        let mut inner = self.inner.lock().await;
        let Some(job) = inner.jobs_by_id.get(job_id) else {
            return false;
        };
        let Some(request_id) = job.request_id.clone() else {
            return false;
        };
        if job.visibility != ShellJobVisibility::HiddenUntilHandoff || !job.lifecycle.is_terminal()
        {
            return false;
        }
        let client_id = job.client_id.clone();
        let runner_instance_id = job.runner_instance_id.clone();
        // Preserve same-instance reconnect suppression whenever the exact
        // Runner lease is still current. If that lease disappeared or was
        // replaced after the terminal snapshot was already projected, do not
        // strand the hidden terminal record forever: a replacement instance
        // cannot consume the retired lease, while a later registration after
        // the Server forgot the lease is conservatively equivalent to fresh
        // recovery and may reconstruct retained Runner terminal evidence.
        if let Some(runner) = inner
            .runners
            .get_mut(&client_id)
            .filter(|runner| runner.runner_instance_id == runner_instance_id)
        {
            // The suppression store predates raw-shell/validation hidden
            // handoff, but its proof is only the exact
            // runner/instance/job/request tuple.
            runner.remember_projected_structured_terminal(
                job_id.to_string(),
                request_id.clone(),
                now_ts(),
            );
        }

        let removed = inner
            .jobs_by_id
            .remove(job_id)
            .expect("projected hidden Job was checked under the registry lock");
        inner.request_to_job.remove(&request_id);
        inner.pending_by_id.remove(&request_id);
        if let Some(queue) = inner.queues_by_runner.get_mut(&removed.client_id) {
            queue.retain(|id| id != &request_id);
        }
        true
    }

    pub async fn remove_projected_hidden_structured_job_record(&self, job_id: &str) -> bool {
        self.remove_projected_hidden_terminal_job_record(job_id)
            .await
    }

    pub async fn get_job(&self, job_id: &str) -> Result<ShellJobInfo, String> {
        self.get_job_for_auth(None, job_id).await
    }

    pub async fn get_job_for_auth(
        &self,
        auth: Option<&crate::RunnerAccess>,
        job_id: &str,
    ) -> Result<ShellJobInfo, String> {
        validate_id(job_id, "job_id")?;
        let mut inner = self.inner.lock().await;
        refresh_job_status_locked(&mut inner, job_id);
        let Some(job) = inner.jobs_by_id.get(job_id) else {
            return Err(format!("unknown shell job: {}", job_id));
        };
        if job.visibility != ShellJobVisibility::Public
            || !shell_job_visible_to_auth(auth, &inner, job)
        {
            return Err(format!("unknown shell job: {}", job_id));
        }
        Ok(job_view(job))
    }

    pub async fn list_jobs(&self, limit: Option<usize>) -> Vec<ShellJobInfo> {
        self.list_jobs_for_auth(None, limit).await
    }

    pub async fn list_jobs_for_auth(
        &self,
        auth: Option<&crate::RunnerAccess>,
        limit: Option<usize>,
    ) -> Vec<ShellJobInfo> {
        self.visible_job_records_for_auth(auth)
            .await
            .into_iter()
            .take(limit.unwrap_or(20).clamp(1, 100))
            .map(|job| job_view(&job))
            .collect()
    }

    /// Complete caller-visible Runner Job set for aggregate observability.
    /// Unlike the public list projection, this has no display pagination:
    /// runtime counts must not silently drop an older active Job behind newer
    /// records. Authorization and public-visibility filtering are identical
    /// to `list_jobs_for_auth`.
    pub async fn list_all_jobs_for_auth(
        &self,
        auth: Option<&crate::RunnerAccess>,
    ) -> Vec<ShellJobInfo> {
        self.visible_job_records_for_auth(auth)
            .await
            .into_iter()
            .map(|job| job_view(&job))
            .collect()
    }

    async fn visible_job_records_for_auth(
        &self,
        auth: Option<&crate::RunnerAccess>,
    ) -> Vec<ShellJobRecord> {
        let mut inner = self.inner.lock().await;
        let job_ids = inner.jobs_by_id.keys().cloned().collect::<Vec<_>>();
        for job_id in job_ids {
            refresh_job_status_locked(&mut inner, &job_id);
        }
        let mut jobs = inner
            .jobs_by_id
            .values()
            .filter(|job| job.visibility == ShellJobVisibility::Public)
            .filter(|job| shell_job_visible_to_auth(auth, &inner, job))
            .cloned()
            .collect::<Vec<_>>();
        jobs.sort_by_key(|job| std::cmp::Reverse(job.created_at));
        jobs
    }

    /// Count active jobs for one exact runtime project without applying the
    /// display-list pagination limit. Jobs without a runtime project id are
    /// intentionally excluded.
    pub async fn count_active_jobs_for_project(
        &self,
        auth: Option<&crate::RunnerAccess>,
        runtime_project_id: &str,
    ) -> usize {
        let mut inner = self.inner.lock().await;
        let job_ids = inner.jobs_by_id.keys().cloned().collect::<Vec<_>>();
        for job_id in job_ids {
            refresh_job_status_locked(&mut inner, &job_id);
        }
        inner
            .jobs_by_id
            .values()
            .filter(|job| job.visibility == ShellJobVisibility::Public)
            .filter(|job| shell_job_visible_to_auth(auth, &inner, job))
            .filter(|job| job.project_id.as_deref() == Some(runtime_project_id))
            .filter(|job| job.lifecycle.is_active())
            .count()
    }

    /// Atomically fence new job starts and count all currently active jobs for
    /// a runtime project. The fence remains until `end_project_unregister`.
    pub async fn begin_project_unregister(
        &self,
        auth: Option<&crate::RunnerAccess>,
        runtime_project_id: &str,
    ) -> Result<usize, String> {
        let mut inner = self.inner.lock().await;
        let job_ids = inner.jobs_by_id.keys().cloned().collect::<Vec<_>>();
        for job_id in job_ids {
            refresh_job_status_locked(&mut inner, &job_id);
        }
        let active = inner
            .jobs_by_id
            .values()
            .filter(|job| shell_job_visible_to_auth(auth, &inner, job))
            .filter(|job| job.project_id.as_deref() == Some(runtime_project_id))
            .filter(|job| job.lifecycle.is_active())
            .count();
        if active == 0 {
            *inner
                .unregistering_projects
                .entry(runtime_project_id.to_string())
                .or_insert(0) += 1;
        }
        Ok(active)
    }

    pub async fn end_project_unregister(&self, runtime_project_id: &str) {
        let mut inner = self.inner.lock().await;
        if let Some(count) = inner.unregistering_projects.get_mut(runtime_project_id) {
            *count -= 1;
            if *count == 0 {
                inner.unregistering_projects.remove(runtime_project_id);
            }
        }
    }

    pub async fn list_jobs_for_runner(
        &self,
        client_id: &str,
        status: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<ShellJobInfo>, String> {
        validate_id(client_id, "client_id")?;
        let mut inner = self.inner.lock().await;
        if !inner.runners.contains_key(client_id) {
            return Err(format!("unknown shell client: {}", client_id));
        }
        let job_ids = inner.jobs_by_id.keys().cloned().collect::<Vec<_>>();
        for job_id in job_ids {
            refresh_job_status_locked(&mut inner, &job_id);
        }
        let mut jobs = inner
            .jobs_by_id
            .values()
            .filter(|job| job.client_id == client_id)
            .filter(|job| {
                status
                    .map(|status| status == job.public_status())
                    .unwrap_or(true)
            })
            .cloned()
            .collect::<Vec<_>>();
        jobs.sort_by_key(|job| std::cmp::Reverse(job.created_at));
        Ok(jobs
            .into_iter()
            .take(limit.unwrap_or(20).clamp(1, 100))
            .map(|job| job_view(&job))
            .collect())
    }

    /// Legacy immediate `job_log`: no wait parameters, returns the current
    /// snapshot and log segment without waiting. The wait outcome is
    /// `immediate`.
    pub async fn job_log(
        &self,
        job_id: &str,
        since_stdout_line: Option<usize>,
        since_stderr_line: Option<usize>,
        tail_lines: Option<usize>,
    ) -> Result<(ShellJobInfo, Option<String>, Option<String>, usize, usize), String> {
        let (job, stdout, stderr, next_stdout_line, next_stderr_line, _wait) = self
            .job_log_for_auth(
                None,
                job_id,
                since_stdout_line,
                since_stderr_line,
                tail_lines,
                None,
                None,
            )
            .await?;
        Ok((job, stdout, stderr, next_stdout_line, next_stderr_line))
    }

    /// Read bounded stdout/stderr for a job, optionally waiting once for the
    /// opaque observation token to change or for the job to become terminal.
    pub async fn job_log_for_auth(
        &self,
        auth: Option<&crate::RunnerAccess>,
        job_id: &str,
        since_stdout_line: Option<usize>,
        since_stderr_line: Option<usize>,
        tail_lines: Option<usize>,
        after_observation_token: Option<&str>,
        wait_secs: Option<u64>,
    ) -> Result<
        (
            ShellJobInfo,
            Option<String>,
            Option<String>,
            usize,
            usize,
            ShellJobLogObservation,
        ),
        String,
    > {
        validate_id(job_id, "job_id")?;
        let after = after_observation_token
            .map(|value| {
                webcodex_core::job_observation::JobObservationToken::parse_bound(value, job_id)
                    .map_err(|error| error.to_string())
            })
            .transpose()?;
        let deadline = wait_secs
            .map(|secs| tokio::time::Instant::now() + tokio::time::Duration::from_secs(secs));
        let mut waited_ms = 0u64;
        let mut waited = false;
        let mut inner = self.inner.lock().await;
        refresh_job_status_locked(&mut inner, job_id);
        loop {
            let Some(job) = inner.jobs_by_id.get(job_id) else {
                return Err(format!("unknown shell job: {}", job_id));
            };
            if job.visibility != ShellJobVisibility::Public
                || !shell_job_visible_to_auth(auth, &inner, job)
            {
                return Err(format!("unknown shell job: {}", job_id));
            }
            let revision = job.observation.revision.load(Ordering::Relaxed);
            let changed = after.as_ref().is_some_and(|token| {
                token.epoch != job.observation.epoch.as_ref() || token.revision != revision
            });
            let terminal = job.lifecycle.is_terminal();
            if wait_secs.is_none() || after.is_none() || changed || terminal {
                let wait_outcome = if changed {
                    if waited {
                        JobLogWaitOutcome::Updated
                    } else {
                        JobLogWaitOutcome::Immediate
                    }
                } else if terminal {
                    JobLogWaitOutcome::Terminal
                } else {
                    JobLogWaitOutcome::Immediate
                };
                let wait = JobLogWait {
                    wait_outcome,
                    waited_ms,
                    changed,
                    terminal,
                };
                return Ok(frozen_shell_job_log_projection(
                    job,
                    after.as_ref(),
                    since_stdout_line,
                    since_stderr_line,
                    tail_lines,
                    wait,
                ));
            }

            let update_notify = job.observation.notify.clone();
            let notified = update_notify.notified();
            drop(inner);

            inner = self.inner.lock().await;
            refresh_job_status_locked(&mut inner, job_id);
            let Some(job) = inner.jobs_by_id.get(job_id) else {
                return Err(format!("unknown shell job: {}", job_id));
            };
            if job.visibility != ShellJobVisibility::Public
                || !shell_job_visible_to_auth(auth, &inner, job)
            {
                return Err(format!("unknown shell job: {}", job_id));
            }
            let revision = job.observation.revision.load(Ordering::Relaxed);
            let changed = after.as_ref().is_some_and(|token| {
                token.epoch != job.observation.epoch.as_ref() || token.revision != revision
            });
            let terminal = job.lifecycle.is_terminal();
            if changed || terminal {
                let wait = JobLogWait {
                    wait_outcome: if terminal {
                        JobLogWaitOutcome::Terminal
                    } else {
                        JobLogWaitOutcome::Updated
                    },
                    waited_ms,
                    changed,
                    terminal,
                };
                return Ok(frozen_shell_job_log_projection(
                    job,
                    after.as_ref(),
                    since_stdout_line,
                    since_stderr_line,
                    tail_lines,
                    wait,
                ));
            }
            drop(inner);

            let wait_started = tokio::time::Instant::now();
            let wake = tokio::time::timeout_at(
                deadline.expect("bounded wait requires a deadline"),
                notified,
            )
            .await;
            waited = true;
            waited_ms = waited_ms.saturating_add(wait_started.elapsed().as_millis() as u64);
            inner = self.inner.lock().await;
            refresh_job_status_locked(&mut inner, job_id);
            if wake.is_err() {
                let Some(job) = inner.jobs_by_id.get(job_id) else {
                    return Err(format!("unknown shell job: {}", job_id));
                };
                if job.visibility != ShellJobVisibility::Public
                    || !shell_job_visible_to_auth(auth, &inner, job)
                {
                    return Err(format!("unknown shell job: {}", job_id));
                }
                let revision = job.observation.revision.load(Ordering::Relaxed);
                let changed = after.as_ref().is_some_and(|token| {
                    token.epoch != job.observation.epoch.as_ref() || token.revision != revision
                });
                let terminal = job.lifecycle.is_terminal();
                let wait = JobLogWait {
                    wait_outcome: if terminal {
                        JobLogWaitOutcome::Terminal
                    } else if changed {
                        JobLogWaitOutcome::Updated
                    } else {
                        JobLogWaitOutcome::Timeout
                    },
                    waited_ms,
                    changed,
                    terminal,
                };
                return Ok(frozen_shell_job_log_projection(
                    job,
                    after.as_ref(),
                    since_stdout_line,
                    since_stderr_line,
                    tail_lines,
                    wait,
                ));
            }
        }
    }

    pub async fn stop_job(
        &self,
        job_id: &str,
        requested_by: String,
    ) -> Result<ShellJobInfo, String> {
        self.stop_job_for_auth(None, job_id, requested_by).await
    }

    pub async fn stop_job_for_auth(
        &self,
        auth: Option<&crate::RunnerAccess>,
        job_id: &str,
        requested_by: String,
    ) -> Result<ShellJobInfo, String> {
        validate_id(job_id, "job_id")?;
        let mut inner = self.inner.lock().await;
        refresh_job_status_locked(&mut inner, job_id);
        let Some(job) = inner.jobs_by_id.get(job_id).cloned() else {
            return Err(format!("unknown shell job: {}", job_id));
        };
        if job.visibility != ShellJobVisibility::Public
            || !shell_job_visible_to_auth(auth, &inner, &job)
        {
            return Err(format!("unknown shell job: {}", job_id));
        }
        if job.recovery_active() {
            return Err(
                "runner_unavailable_recovering: wait for same-instance job reconciliation before retrying stop_job"
                    .to_string(),
            );
        }
        match job.lifecycle {
            JobLifecycleState::Queued => {
                if let Some(request_id) = &job.request_id {
                    remove_pending_request_locked(&mut inner, request_id);
                    inner.request_to_job.remove(request_id);
                }
                let job = inner.jobs_by_id.get_mut(job_id).expect("job exists");
                let terminal_now = now_ts();
                job.lifecycle = JobLifecycleState::Stopped;
                observe_job_terminal(job, terminal_now);
                job.ended_at = Some(terminal_now);
                job.error = Some("job stopped before Runner picked it up".to_string());
                if job.structured_execution.is_some() {
                    job.command_execution_state = Some(ShellCommandExecutionState::NotStarted);
                }
                notify_job_update(job);
                Ok(job_view(job))
            }
            JobLifecycleState::StopRequested => {
                Ok(job_view(inner.jobs_by_id.get(job_id).expect("job exists")))
            }
            JobLifecycleState::RunnerQueued | JobLifecycleState::Running => {
                let stop_request_id = next_request_id();
                let client_id = job.client_id.clone();
                let request = RunnerRequest::from_operation(
                    RunnerInvocationMetadata {
                        request_id: stop_request_id.clone(),
                        client_id: client_id.clone(),
                        requested_by,
                        created_at: now_ts(),
                    },
                    RunnerOperation::Job(RunnerJobOperation::Stop {
                        job_id: job_id.to_string(),
                    }),
                )?;
                enqueue_pending_request_locked(
                    self.telemetry.as_ref(),
                    &mut inner,
                    &client_id,
                    stop_request_id,
                    request,
                    None,
                    Some(job_id.to_string()),
                )?;
                let job = inner.jobs_by_id.get_mut(job_id).expect("job exists");
                job.lifecycle = JobLifecycleState::StopRequested;
                job.error = Some("stop requested".to_string());
                notify_job_update(job);
                let notify_runner_id = job.client_id.clone();
                notify_runner_locked(&inner, &notify_runner_id);
                Ok(job_view(inner.jobs_by_id.get(job_id).expect("job exists")))
            }
            _ => Ok(job_view(inner.jobs_by_id.get(job_id).expect("job exists"))),
        }
    }

    /// Polling-transport job update entry point. Job ownership and update
    /// sequence rules decide acceptance; this path refreshes `last_seen` for
    /// the active instance. Used by the HTTP `/job_update` handler.
    pub async fn update_job(&self, body: RunnerJobUpdateRequest) -> Result<ShellJobInfo, String> {
        self.update_job_checked(body, None).await
    }

    /// Connection-scoped job update entry point for long-lived transports.
    /// Acceptance still follows the existing `agent_instance_id`, job
    /// ownership and `update_seq` rules so a legitimately-dispatched job's
    /// late update is not dropped just because the transport connection was
    /// replaced. A late update arriving on a stale same-instance connection,
    /// however, must not refresh the new connection's `last_seen` liveness.
    pub async fn update_job_for_connection(
        &self,
        body: RunnerJobUpdateRequest,
        connection_id: &str,
    ) -> Result<ShellJobInfo, String> {
        self.update_job_checked(body, Some(connection_id)).await
    }

    async fn update_job_checked(
        &self,
        body: RunnerJobUpdateRequest,
        expected_connection_id: Option<&str>,
    ) -> Result<ShellJobInfo, String> {
        validate_id(&body.client_id, "client_id")?;
        validate_id(&body.job_id, "job_id")?;
        validate_runner_instance_id(&body.runner_instance_id)?;
        if let Some(snapshot) = body.log_snapshot.as_ref() {
            validate_stream_snapshot(&snapshot.stdout, "job update stdout snapshot")?;
            validate_stream_snapshot(&snapshot.stderr, "job update stderr snapshot")?;
            if body.stdout_chunk.is_some()
                || body.stderr_chunk.is_some()
                || body.stdout_tail.is_some()
                || body.stderr_tail.is_some()
            {
                return Err(
                    "job update log_snapshot cannot be combined with chunk or legacy tail fields"
                        .to_string(),
                );
            }
        }
        let mut inner = self.inner.lock().await;
        // Reject job updates from a stale/replaced instance before refreshing
        // liveness or mutating job state.
        assert_active_instance_locked(&inner, &body.client_id, &body.runner_instance_id)?;
        let sequenced = inner.runners.get(&body.client_id).is_some_and(|runner| {
            runner
                .runner_features
                .supports(RunnerFeature::JobStateReconciliation)
        });
        let incoming_seq = if sequenced {
            Some(
                body.update_seq
                    .filter(|sequence| *sequence > 0)
                    .ok_or_else(|| {
                        "job_state_reconciliation update requires update_seq".to_string()
                    })?,
            )
        } else {
            body.update_seq
        };
        let incoming_status = body.status.trim();
        if sequenced && body.status != incoming_status {
            return Err(
                "job_state_reconciliation update status must be canonical without surrounding whitespace"
                    .to_string(),
            );
        }
        let incoming_lifecycle = parse_job_lifecycle(incoming_status).map_err(|_| {
            if sequenced {
                format!(
                    "job_state_reconciliation update status '{}' is invalid",
                    incoming_status
                )
            } else {
                format!("job update status '{}' is invalid", incoming_status)
            }
        })?;
        if sequenced && !(incoming_lifecycle.is_runner_active() || incoming_lifecycle.is_terminal())
        {
            return Err(format!(
                "job_state_reconciliation update status '{}' is invalid",
                incoming_status
            ));
        }
        if sequenced && body.finished != incoming_lifecycle.is_terminal() {
            return Err(
                "job_state_reconciliation update has inconsistent finished/status".to_string(),
            );
        }
        if sequenced
            && !incoming_lifecycle.is_terminal()
            && (body.exit_code.is_some() || body.duration_ms.is_some())
        {
            return Err(
                "active job_state_reconciliation update contains terminal result fields"
                    .to_string(),
            );
        }
        if sequenced
            && incoming_lifecycle == JobLifecycleState::Completed
            && body.exit_code != Some(0)
        {
            return Err(
                "completed job_state_reconciliation update requires exit_code=0".to_string(),
            );
        }
        // Refresh liveness only for the connection that currently holds the
        // transport lease. A late job update on a stale same-instance
        // connection is still applied below, but it must not make the new
        // connection appear online.
        if expected_connection_id.is_none()
            || inner
                .runners
                .get(&body.client_id)
                .is_some_and(|runner| runner.connection_id.as_deref() == expected_connection_id)
        {
            if let Some(runner) = inner.runners.get_mut(&body.client_id) {
                runner.last_seen = now_ts();
            }
        }
        let mut request_id_to_remove = None;
        let remove_cleanup_terminal;
        let view = {
            let Some(job) = inner.jobs_by_id.get_mut(&body.job_id) else {
                return Err(format!("unknown shell job: {}", body.job_id));
            };
            if job.client_id != body.client_id {
                return Err("job_id does not belong to client_id".to_string());
            }
            if job.runner_instance_id != body.runner_instance_id {
                return Err("job_id belongs to a replaced runner instance".to_string());
            }
            if body
                .request_id
                .as_deref()
                .is_some_and(|request_id| job.request_id.as_deref() != Some(request_id))
            {
                return Err("job update request_id does not match job_id".to_string());
            }
            if body.log_snapshot.is_some() && !sequenced {
                return Err(
                    "job update log_snapshot requires job_state_reconciliation capability"
                        .to_string(),
                );
            }
            if job.recovery_active() && sequenced && body.log_snapshot.is_none() {
                return Err(
                    "recovering job update requires an authoritative log_snapshot or register inventory"
                        .to_string(),
                );
            }
            let before = public_mutation_signature(job);
            if sequenced && incoming_seq.is_some_and(|sequence| sequence <= job.last_update_seq) {
                return Ok(job_view(job));
            }
            if job.lifecycle.is_terminal() {
                // Terminal class is server-authoritative once accepted.
                return Ok(job_view(job));
            }
            if body.log_snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.stdout.next_line < job.stdout.next_line
                    || snapshot.stderr.next_line < job.stderr.next_line
            }) {
                return Err(
                    "job update authoritative log snapshot regresses an absolute cursor"
                        .to_string(),
                );
            }
            self.telemetry.runner_job_update_accepted(
                body.request_id.as_deref(),
                &body.job_id,
                &body,
            );
            if let Err(error) = validate_validation_progress(job, &body, incoming_lifecycle)
                .and_then(|_| validate_command_execution_state(job, &body, incoming_lifecycle))
                .and_then(|_| validate_job_activity(job, &body, incoming_lifecycle))
            {
                let terminal_now = now_ts();
                job.lifecycle = JobLifecycleState::Failed;
                observe_job_terminal(job, terminal_now);
                job.ended_at = Some(terminal_now);
                job.exit_code = body.exit_code;
                job.duration_ms = body.duration_ms;
                job.error = Some(format!("executor protocol violation: {}", error.0));
                job.activity = None;
                if job.structured_execution.is_some() {
                    job.command_execution_state = Some(if job.started_at.is_some() {
                        ShellCommandExecutionState::OutcomeUnknown
                    } else {
                        ShellCommandExecutionState::NotStarted
                    });
                }
                request_id_to_remove = job.request_id.clone();
            } else {
                let was_recovering = job.recovery_active();
                if let Some(snapshot) = body.log_snapshot {
                    super::jobs::replace_log_from_snapshot(&mut job.stdout, &snapshot.stdout);
                    super::jobs::replace_log_from_snapshot(&mut job.stderr, &snapshot.stderr);
                } else {
                    replace_log_limited(&mut job.stdout, body.stdout_tail);
                    replace_log_limited(&mut job.stderr, body.stderr_tail);
                    append_log_limited(&mut job.stdout, body.stdout_chunk);
                    append_log_limited(&mut job.stderr, body.stderr_chunk);
                }
                if body.validation_progress.is_some() {
                    job.validation_progress = body.validation_progress.clone();
                }
                if body.activity.is_some() {
                    job.activity = body.activity;
                }
                if job.started_at.is_none()
                    && body.command_execution_state != Some(ShellCommandExecutionState::NotStarted)
                    && matches!(
                        incoming_lifecycle,
                        JobLifecycleState::Running
                            | JobLifecycleState::Completed
                            | JobLifecycleState::Failed
                            | JobLifecycleState::Stopped
                            | JobLifecycleState::Timeout
                    )
                {
                    job.started_at = Some(now_ts());
                }
                if !job.lifecycle.is_terminal() {
                    job.lifecycle = if incoming_lifecycle == JobLifecycleState::Queued
                        && job.started_at.is_some()
                    {
                        JobLifecycleState::RunnerQueued
                    } else {
                        incoming_lifecycle
                    };
                }
                if incoming_lifecycle.is_terminal() {
                    let terminal_now = now_ts();
                    job.lifecycle = incoming_lifecycle;
                    observe_job_terminal(job, terminal_now);
                    job.ended_at = Some(terminal_now);
                    job.exit_code = body.exit_code;
                    job.duration_ms = body.duration_ms;
                    job.error = body.error;
                    job.command_execution_state = body.command_execution_state;
                    job.activity = None;
                    if !was_recovering {
                        job.recovery.phase = job
                            .recovery
                            .reconciled_at
                            .map(|_| JobRecoveryPhase::Reconciled);
                    }
                    job.recovery.recovering_since = None;
                    request_id_to_remove = job.request_id.clone();
                } else if body.error.is_some() {
                    job.error = body.error;
                }
                if body.finished && !job.lifecycle.is_terminal() {
                    let terminal_now = now_ts();
                    job.lifecycle = if job.error.is_none() && job.exit_code == Some(0) {
                        JobLifecycleState::Completed
                    } else {
                        JobLifecycleState::Failed
                    };
                    observe_job_terminal(job, terminal_now);
                    job.ended_at = Some(terminal_now);
                    job.activity = None;
                    request_id_to_remove = job.request_id.clone();
                }
                if was_recovering {
                    job.recovery.phase = Some(JobRecoveryPhase::Reconciled);
                    job.recovery.reconciled_at = Some(now_ts());
                    job.recovery.reason = Some(JobRecoveryReason::SameInstanceUpdateReconciliation);
                    job.recovery.recovering_since = None;
                }
            }
            if let Some(sequence) = incoming_seq {
                job.last_update_seq = sequence;
            }
            if public_mutation_signature(job) != before {
                notify_job_update(job);
            }
            remove_cleanup_terminal =
                job.visibility == ShellJobVisibility::CleanupPending && job.lifecycle.is_terminal();
            job_view(job)
        };
        if let Some(request_id) = request_id_to_remove {
            inner.pending_by_id.remove(&request_id);
            inner.request_to_job.remove(&request_id);
        }
        if remove_cleanup_terminal {
            inner.jobs_by_id.remove(&body.job_id);
        }
        if is_final_job_status(&view.status) {
            self.telemetry
                .runner_job_finalized(body.request_id.as_deref(), &body.job_id);
        }
        Ok(view)
    }
}
