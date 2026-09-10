use super::jobs::{
    command_preview, is_final_job_status, is_runner_active_job_status, mark_job_lost,
    notify_job_update, observe_job_terminal, parse_job_lifecycle, replace_log_from_snapshot,
    COMMAND_PREVIEW_MAX_CHARS,
};
use super::state::{
    JobLifecycleState, JobObservationState, JobRecoveryPhase, JobRecoveryReason, JobRecoveryState,
    RunnerRegistryInner, ShellJobLogState, ShellJobRecord, ShellJobVisibility,
};
use super::validation::validate_id;
use super::{job_recovery_grace_secs, RunnerRegistry};
use crate::RunnerAccessGroup;
use std::collections::HashSet;
use webcodex_core::runner_protocol::{
    RunnerProjectSummary, ShellCommandExecutionState, ShellJobActivityPhase,
    ShellJobActivitySource, ShellJobInventory, ShellJobSnapshot, ShellJobStreamSnapshot,
    JOB_INVENTORY_MAX_ACTIVE_JOBS, JOB_INVENTORY_MAX_JOBS, JOB_INVENTORY_MAX_SERIALIZED_BYTES,
    JOB_INVENTORY_MAX_TERMINAL_JOBS, JOB_SNAPSHOT_STREAM_MAX_BYTES, JOB_TERMINAL_RETENTION_SECS,
};

const MAX_CONTEXT_FIELD_CHARS: usize = 1_024;
const MAX_SNAPSHOT_ERROR_CHARS: usize = 4_096;

fn valid_bounded_text(value: &str, max_chars: usize) -> bool {
    !value.contains('\0') && value.chars().count() <= max_chars
}

fn retained_line_count(value: &str) -> usize {
    value.lines().count()
}

pub(super) fn validate_stream_snapshot(
    stream: &ShellJobStreamSnapshot,
    field: &str,
) -> Result<(), String> {
    if stream.tail.len() > JOB_SNAPSHOT_STREAM_MAX_BYTES {
        return Err(format!(
            "job inventory {field} exceeds {} bytes",
            JOB_SNAPSHOT_STREAM_MAX_BYTES
        ));
    }
    if stream.first_retained_line == 0
        || stream.next_line
            != stream
                .first_retained_line
                .saturating_add(retained_line_count(&stream.tail))
    {
        return Err(format!("job inventory {field} line range is inconsistent"));
    }
    if stream.first_retained_line > 1 && !stream.truncated {
        return Err(format!(
            "job inventory {field} omits earlier lines without truncated=true"
        ));
    }
    Ok(())
}

fn validate_context(
    client_id: &str,
    projects: &[RunnerProjectSummary],
    require_project_membership: bool,
    snapshot: &ShellJobSnapshot,
) -> Result<(), String> {
    let context = &snapshot.context;
    if command_preview(&context.command_preview) != context.command_preview
        || context.command_preview.chars().count() > COMMAND_PREVIEW_MAX_CHARS + 1
    {
        return Err("job inventory command_preview is not canonical and bounded".to_string());
    }
    for (name, value) in [
        ("project_cwd", context.project_cwd.as_deref()),
        ("cwd", context.cwd.as_deref()),
        ("purpose", context.purpose.as_deref()),
        ("shell", context.shell.as_deref()),
    ] {
        if value.is_some_and(|value| !valid_bounded_text(value, MAX_CONTEXT_FIELD_CHARS)) {
            return Err(format!("job inventory {name} is invalid or oversized"));
        }
    }
    if context.purpose.as_deref().is_some_and(|purpose| {
        !matches!(
            purpose,
            "validation"
                | "test"
                | "build"
                | "format"
                | "release"
                | "diagnostic"
                | "operation"
                | "other"
        )
    }) {
        return Err("job inventory purpose is invalid".to_string());
    }
    if context.shell.as_deref().is_some_and(|shell| {
        !matches!(
            shell,
            "sh" | "bash"
                | "powershell"
                | "javascript"
                | "typescript"
                | "direct_argv"
                | "configured"
                | "custom"
                | "remote"
        )
    }) {
        return Err("job inventory shell is invalid".to_string());
    }
    if context.validation_steps.len() > 3
        || context
            .validation_steps
            .iter()
            .collect::<HashSet<_>>()
            .len()
            != context.validation_steps.len()
        || context
            .validation_steps
            .iter()
            .any(|step| !matches!(step.as_str(), "format" | "check" | "test"))
    {
        return Err("job inventory validation_steps are invalid".to_string());
    }
    if context.validation.as_ref().is_some_and(|validation| {
        !validation.is_valid()
            || validation
                .steps
                .iter()
                .map(|step| step.name.clone())
                .collect::<Vec<_>>()
                != context.validation_steps
    }) {
        return Err("job inventory validation metadata is invalid".to_string());
    }
    if context
        .structured_execution
        .as_ref()
        .is_some_and(|metadata| !metadata.is_valid())
        || (context.structured_execution.is_some()
            && (!context.validation_steps.is_empty() || context.validation.is_some()))
    {
        return Err("job inventory structured execution metadata is invalid".to_string());
    }
    if context
        .workflow_session_id
        .as_deref()
        .is_some_and(|session_id| {
            !webcodex_core::workflow_session_contract::is_valid_session_id(session_id)
                || session_id.chars().count() > 128
        })
    {
        return Err("job inventory workflow_session_id is invalid".to_string());
    }
    if context.workflow_session_id.is_some() && context.runtime_project_id.is_none() {
        return Err("job inventory workflow_session_id requires runtime_project_id".to_string());
    }
    if let Some(runtime_project_id) = context.runtime_project_id.as_deref() {
        let prefix = format!("agent:{client_id}:");
        let project_id = runtime_project_id
            .strip_prefix(&prefix)
            .filter(|project_id| !project_id.is_empty())
            .ok_or_else(|| {
                "job inventory runtime_project_id does not belong to client_id".to_string()
            })?;
        validate_id(project_id, "project_id")?;
        if require_project_membership
            && !projects
                .iter()
                .any(|project| !project.disabled && project.id == project_id)
        {
            return Err(
                "job inventory runtime_project_id is not registered by this runner".to_string(),
            );
        }
    }
    Ok(())
}

fn validate_snapshot(
    client_id: &str,
    projects: &[RunnerProjectSummary],
    require_project_membership: bool,
    snapshot: &ShellJobSnapshot,
) -> Result<bool, String> {
    validate_id(&snapshot.job_id, "job_id")?;
    validate_id(&snapshot.request_id, "request_id")?;
    if snapshot.update_seq == 0 {
        return Err("job inventory update_seq must be greater than zero".to_string());
    }
    let lifecycle = parse_job_lifecycle(&snapshot.status)
        .map_err(|_| format!("job inventory status '{}' is invalid", snapshot.status))?;
    let active = lifecycle.is_runner_active();
    let terminal = lifecycle.is_terminal();
    if !active && !terminal {
        return Err(format!(
            "job inventory status '{}' is invalid",
            snapshot.status
        ));
    }
    if snapshot.created_at <= 0
        || snapshot
            .started_at
            .is_some_and(|started| started < snapshot.created_at)
        || snapshot
            .ended_at
            .is_some_and(|ended| ended < snapshot.started_at.unwrap_or(snapshot.created_at))
    {
        return Err("job inventory timestamps are inconsistent".to_string());
    }
    if active && snapshot.ended_at.is_some() {
        return Err("active job inventory snapshot must not have ended_at".to_string());
    }
    if active && (snapshot.exit_code.is_some() || snapshot.duration_ms.is_some()) {
        return Err(
            "active job inventory snapshot must not contain terminal result fields".to_string(),
        );
    }
    if terminal && snapshot.ended_at.is_none() {
        return Err("terminal job inventory snapshot requires ended_at".to_string());
    }
    if lifecycle == JobLifecycleState::Completed && snapshot.exit_code != Some(0) {
        return Err("completed job inventory snapshot requires exit_code=0".to_string());
    }
    if matches!(
        lifecycle,
        JobLifecycleState::Running | JobLifecycleState::StopRequested
    ) && snapshot.started_at.is_none()
    {
        return Err("running job inventory snapshot requires started_at".to_string());
    }
    if snapshot
        .error
        .as_deref()
        .is_some_and(|error| !valid_bounded_text(error, MAX_SNAPSHOT_ERROR_CHARS))
    {
        return Err("job inventory error is invalid or oversized".to_string());
    }
    if active && snapshot.command_execution_state.is_some() {
        return Err(
            "active job inventory snapshot must not contain terminal lifecycle".to_string(),
        );
    }
    if snapshot.context.structured_execution.is_some()
        && terminal
        && snapshot.command_execution_state.is_none()
    {
        return Err(
            "terminal structured job inventory snapshot requires command_execution_state"
                .to_string(),
        );
    }
    if let Some(state) = snapshot.command_execution_state {
        let consistent = match state {
            ShellCommandExecutionState::NotStarted => {
                snapshot.started_at.is_none()
                    && matches!(
                        lifecycle,
                        JobLifecycleState::Failed
                            | JobLifecycleState::Stopped
                            | JobLifecycleState::Cancelled
                            | JobLifecycleState::Lost
                    )
            }
            ShellCommandExecutionState::OutcomeUnknown => {
                matches!(
                    lifecycle,
                    JobLifecycleState::Failed | JobLifecycleState::Lost
                )
            }
            ShellCommandExecutionState::TimedOut => {
                matches!(
                    lifecycle,
                    JobLifecycleState::Timeout | JobLifecycleState::TimedOut
                )
            }
            ShellCommandExecutionState::Completed => matches!(
                lifecycle,
                JobLifecycleState::Completed
                    | JobLifecycleState::Failed
                    | JobLifecycleState::Stopped
                    | JobLifecycleState::Cancelled
            ),
        };
        if !consistent {
            return Err("job inventory command_execution_state is inconsistent".to_string());
        }
    }
    validate_context(client_id, projects, require_project_membership, snapshot)?;
    validate_stream_snapshot(&snapshot.stdout, "stdout")?;
    validate_stream_snapshot(&snapshot.stderr, "stderr")?;
    if snapshot.context.validation_steps.is_empty() && snapshot.validation_progress.is_some() {
        return Err("job inventory validation_progress is unexpected".to_string());
    }
    if let Some(progress) = snapshot.validation_progress.as_ref() {
        if progress.completed > snapshot.context.validation_steps.len()
            || progress
                .current_step
                .as_ref()
                .is_some_and(|step| !snapshot.context.validation_steps.contains(step))
            || progress
                .failed_step
                .as_ref()
                .is_some_and(|step| !snapshot.context.validation_steps.contains(step))
            || (progress.current_step.is_some() && progress.failed_step.is_some())
        {
            return Err("job inventory validation_progress is inconsistent".to_string());
        }
    }
    if !snapshot.context.validation_steps.is_empty() {
        let steps = &snapshot.context.validation_steps;
        let progress = snapshot.validation_progress.as_ref();
        let structurally_valid = match lifecycle {
            JobLifecycleState::RunnerQueued => progress.is_none(),
            JobLifecycleState::Running | JobLifecycleState::StopRequested => {
                progress.is_some_and(|progress| {
                    progress.completed < steps.len()
                        && progress.current_step.as_ref() == steps.get(progress.completed)
                        && progress.failed_step.is_none()
                })
            }
            JobLifecycleState::Completed => progress.is_some_and(|progress| {
                progress.completed == steps.len()
                    && progress.current_step.is_none()
                    && progress.failed_step.is_none()
            }),
            JobLifecycleState::Failed => progress.is_none_or(|progress| {
                progress.current_step.is_none()
                    && progress.failed_step.as_ref().is_none_or(|failed| {
                        progress.completed < steps.len()
                            && steps.get(progress.completed) == Some(failed)
                    })
            }),
            JobLifecycleState::Stopped
            | JobLifecycleState::Timeout
            | JobLifecycleState::TimedOut
            | JobLifecycleState::Cancelled
            | JobLifecycleState::Lost => progress.is_none_or(|progress| {
                progress.current_step.is_none() && progress.failed_step.is_none()
            }),
            _ => false,
        };
        if !structurally_valid {
            return Err("job inventory validation_progress does not match status".to_string());
        }
    }
    if let Some(activity) = snapshot.activity {
        if !activity.is_canonical()
            || !matches!(
                lifecycle,
                JobLifecycleState::Running | JobLifecycleState::StopRequested
            )
        {
            return Err("job inventory activity is invalid for status".to_string());
        }
        match activity.source {
            ShellJobActivitySource::RunnerExecution => {
                if !snapshot.context.validation_steps.is_empty()
                    || activity.phase != ShellJobActivityPhase::ProcessRunning
                {
                    return Err(
                        "job inventory activity is inconsistent with execution kind".to_string()
                    );
                }
            }
            ShellJobActivitySource::ValidationPlan => {
                let expected = match snapshot
                    .validation_progress
                    .as_ref()
                    .and_then(|progress| progress.current_step.as_deref())
                {
                    Some("format") => Some(ShellJobActivityPhase::ValidationFormat),
                    Some("check") => Some(ShellJobActivityPhase::ValidationCheck),
                    Some("test") => Some(ShellJobActivityPhase::ValidationTest),
                    _ => None,
                };
                if expected != Some(activity.phase) {
                    return Err(
                        "job inventory activity is inconsistent with validation progress"
                            .to_string(),
                    );
                }
            }
            ShellJobActivitySource::CargoOutput => {
                let Some(progress) = snapshot.validation_progress.as_ref() else {
                    return Err(
                        "job inventory Cargo activity requires validation progress".to_string()
                    );
                };
                if progress.current_step.is_none() || snapshot.context.validation_steps.is_empty() {
                    return Err(
                        "job inventory Cargo activity requires an active validation step"
                            .to_string(),
                    );
                }
                if let Some(validation) = snapshot.context.validation.as_ref() {
                    if validation
                        .steps
                        .get(progress.completed)
                        .is_none_or(|step| step.program != "cargo" || !step.is_canonical())
                    {
                        return Err(
                            "job inventory Cargo activity is inconsistent with validation metadata"
                                .to_string(),
                        );
                    }
                }
            }
        }
    }
    Ok(active)
}

fn validate_job_inventory_inner(
    client_id: &str,
    projects: &[RunnerProjectSummary],
    require_project_membership: bool,
    inventory: &ShellJobInventory,
) -> Result<(), String> {
    if !inventory.active_complete {
        return Err("job_state_reconciliation requires active_complete job inventory".to_string());
    }
    if inventory.jobs.len() > JOB_INVENTORY_MAX_JOBS {
        return Err(format!(
            "job inventory exceeds {} records",
            JOB_INVENTORY_MAX_JOBS
        ));
    }
    let serialized = serde_json::to_vec(inventory)
        .map_err(|error| format!("could not serialize job inventory: {error}"))?;
    if serialized.len() > JOB_INVENTORY_MAX_SERIALIZED_BYTES {
        return Err(format!(
            "job inventory exceeds {} serialized bytes",
            JOB_INVENTORY_MAX_SERIALIZED_BYTES
        ));
    }
    let mut ids = HashSet::with_capacity(inventory.jobs.len());
    let mut request_ids = HashSet::with_capacity(inventory.jobs.len());
    let mut active_count = 0usize;
    let mut terminal_count = 0usize;
    let mut saw_terminal = false;
    for snapshot in &inventory.jobs {
        if !ids.insert(snapshot.job_id.as_str()) {
            return Err(format!(
                "job inventory contains duplicate job_id {}",
                snapshot.job_id
            ));
        }
        if !request_ids.insert(snapshot.request_id.as_str()) {
            return Err(format!(
                "job inventory contains duplicate request_id {}",
                snapshot.request_id
            ));
        }
        let active = validate_snapshot(client_id, projects, require_project_membership, snapshot)?;
        if active {
            if saw_terminal {
                return Err(
                    "job inventory must order all active jobs before terminal history".to_string(),
                );
            }
            active_count += 1;
        } else {
            saw_terminal = true;
            terminal_count += 1;
        }
    }
    if active_count > JOB_INVENTORY_MAX_ACTIVE_JOBS {
        return Err(format!(
            "job inventory exceeds {} active records",
            JOB_INVENTORY_MAX_ACTIVE_JOBS
        ));
    }
    if terminal_count > JOB_INVENTORY_MAX_TERMINAL_JOBS {
        return Err(format!(
            "job inventory exceeds {} terminal records",
            JOB_INVENTORY_MAX_TERMINAL_JOBS
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn validate_job_inventory(
    client_id: &str,
    projects: &[RunnerProjectSummary],
    inventory: &ShellJobInventory,
) -> Result<(), String> {
    validate_job_inventory_inner(client_id, projects, true, inventory)
}

pub(super) fn validate_job_inventory_without_project_membership(
    client_id: &str,
    inventory: &ShellJobInventory,
) -> Result<(), String> {
    validate_job_inventory_inner(client_id, &[], false, inventory)
}

fn same_context(job: &ShellJobRecord, snapshot: &ShellJobSnapshot) -> bool {
    let context = &snapshot.context;
    job.request_id.as_deref() == Some(snapshot.request_id.as_str())
        && job.project_id == context.runtime_project_id
        && job.session_id == context.workflow_session_id
        && job.ssh_resource == context.ssh_resource
        && job.cwd == context.cwd
        && job.project_cwd == context.project_cwd
        && job.purpose == context.purpose
        && job.shell == context.shell
        && job.command_preview == context.command_preview
        && job.validation_steps == context.validation_steps
        && job.validation == context.validation
        && job.structured_execution == context.structured_execution
}

fn detached_instance_transfer_allowed(job: &ShellJobRecord, snapshot: &ShellJobSnapshot) -> bool {
    job.kind == "run_detached_process"
        && job
            .structured_execution
            .as_ref()
            .is_some_and(|metadata| metadata.execution_source == "run_detached_process")
        && snapshot
            .context
            .structured_execution
            .as_ref()
            .is_some_and(|metadata| metadata.execution_source == "run_detached_process")
        && same_context(job, snapshot)
}

pub(super) fn preflight_inventory_locked(
    inner: &RunnerRegistryInner,
    client_id: &str,
    runner_instance_id: &str,
    inventory: &ShellJobInventory,
) -> Result<(), String> {
    for snapshot in &inventory.jobs {
        if inner
            .request_to_job
            .get(&snapshot.request_id)
            .is_some_and(|job_id| job_id != &snapshot.job_id)
        {
            return Err(format!(
                "job inventory request_id {} belongs to a different job",
                snapshot.request_id
            ));
        }
        let Some(existing) = inner.jobs_by_id.get(&snapshot.job_id) else {
            continue;
        };
        if existing.client_id != client_id {
            return Err(format!(
                "job inventory job_id {} belongs to a different runner",
                snapshot.job_id
            ));
        }
        let detached_instance_transfer = existing.runner_instance_id != runner_instance_id
            && detached_instance_transfer_allowed(existing, snapshot);
        if existing.runner_instance_id != runner_instance_id && !detached_instance_transfer {
            return Err(format!(
                "job inventory job_id {} belongs to a replaced runner instance",
                snapshot.job_id
            ));
        }
        if !same_context(existing, snapshot) {
            return Err(format!(
                "job inventory job_id {} has inconsistent ownership context",
                snapshot.job_id
            ));
        }
        if detached_instance_transfer
            && !existing.lifecycle.is_terminal()
            && snapshot.update_seq < existing.last_update_seq
        {
            return Err(format!(
                "job inventory job_id {} regresses update sequence across detached runner replacement",
                snapshot.job_id
            ));
        }
        let would_apply = snapshot.update_seq > existing.last_update_seq
            || (snapshot.update_seq == existing.last_update_seq && existing.recovery_active());
        if !existing.lifecycle.is_terminal() && would_apply {
            let existing_progress = existing
                .validation_progress
                .as_ref()
                .map(|progress| progress.completed)
                .unwrap_or(0);
            let snapshot_progress = snapshot
                .validation_progress
                .as_ref()
                .map(|progress| progress.completed)
                .unwrap_or(0);
            if snapshot_progress < existing_progress {
                return Err(format!(
                    "job inventory job_id {} regresses validation progress",
                    snapshot.job_id
                ));
            }
            if snapshot.stdout.next_line < existing.stdout.next_line
                || snapshot.stderr.next_line < existing.stderr.next_line
            {
                return Err(format!(
                    "job inventory job_id {} regresses an absolute log cursor",
                    snapshot.job_id
                ));
            }
        }
    }
    Ok(())
}

fn remove_job_request_mapping(
    inner: &mut RunnerRegistryInner,
    client_id: &str,
    request_id: Option<&str>,
) {
    let Some(request_id) = request_id else {
        return;
    };
    inner.pending_by_id.remove(request_id);
    inner.persistent_waiters.remove(request_id);
    inner.request_to_job.remove(request_id);
    if let Some(queue) = inner.queues_by_runner.get_mut(client_id) {
        queue.retain(|queued| queued != request_id);
    }
}

fn remove_job_control_requests(
    inner: &mut RunnerRegistryInner,
    client_id: &str,
    job_ids: &HashSet<String>,
) {
    let request_ids = inner
        .pending_by_id
        .iter()
        .filter(|(_, pending)| {
            pending.request.client_id == client_id
                && pending
                    .job_id
                    .as_ref()
                    .is_some_and(|job_id| job_ids.contains(job_id))
        })
        .map(|(request_id, _)| request_id.clone())
        .collect::<Vec<_>>();
    for request_id in request_ids {
        remove_job_request_mapping(inner, client_id, Some(&request_id));
    }
}

fn record_from_snapshot(
    client_id: &str,
    runner_instance_id: &str,
    auth_group: Option<RunnerAccessGroup>,
    observation_epoch: std::sync::Arc<str>,
    snapshot: &ShellJobSnapshot,
    now: i64,
) -> ShellJobRecord {
    let context = &snapshot.context;
    let mut record = ShellJobRecord {
        job_id: snapshot.job_id.clone(),
        request_id: Some(snapshot.request_id.clone()),
        client_id: client_id.to_string(),
        auth_group,
        runner_instance_id: runner_instance_id.to_string(),
        kind: context
            .structured_execution
            .as_ref()
            .map(|metadata| metadata.execution_source.clone())
            .unwrap_or_else(|| "shell".to_string()),
        project_id: context.runtime_project_id.clone(),
        session_id: context.workflow_session_id.clone(),
        ssh_resource: context.ssh_resource.clone(),
        cwd: context.cwd.clone(),
        project_cwd: context.project_cwd.clone(),
        purpose: context.purpose.clone(),
        shell: context.shell.clone(),
        command_preview: context.command_preview.clone(),
        // Exact replay intent is process-local only and never restored from
        // Runner inventory. Same-key retries after Server restart recover this
        // logical Job instead of guessing that a resent body matches.
        detached_idempotency_intent: None,
        lifecycle: JobLifecycleState::from_wire(&snapshot.status)
            .expect("validated job inventory lifecycle"),
        created_at: snapshot.created_at,
        started_at: snapshot.started_at,
        ended_at: snapshot.ended_at,
        exit_code: snapshot.exit_code,
        duration_ms: snapshot.duration_ms,
        stdout: ShellJobLogState::default(),
        stderr: ShellJobLogState::default(),
        error: snapshot.error.clone(),
        command_execution_state: snapshot.command_execution_state,
        structured_execution: context.structured_execution.clone(),
        codex: None,
        validation_steps: context.validation_steps.clone(),
        validation: context.validation.clone(),
        validation_progress: snapshot.validation_progress.clone(),
        activity: snapshot.activity,
        visibility: super::state::ShellJobVisibility::Public,
        last_update_seq: snapshot.update_seq,
        recovery: JobRecoveryState {
            phase: Some(JobRecoveryPhase::Reconciled),
            recovered_after_server_restart: true,
            reconciled_at: Some(now),
            reason: Some(JobRecoveryReason::ServerRestartReconciliation),
            recovering_since: None,
        },
        observation: JobObservationState::new(observation_epoch),
    };
    observe_job_terminal(&mut record, now);
    record
}

fn apply_snapshot(
    job: &mut ShellJobRecord,
    snapshot: &ShellJobSnapshot,
    now: i64,
    recovery_reason: JobRecoveryReason,
) {
    job.lifecycle =
        JobLifecycleState::from_wire(&snapshot.status).expect("validated job inventory lifecycle");
    observe_job_terminal(job, now);
    job.started_at = snapshot.started_at;
    job.ended_at = snapshot.ended_at;
    job.exit_code = snapshot.exit_code;
    job.duration_ms = snapshot.duration_ms;
    job.error = snapshot.error.clone();
    job.command_execution_state = snapshot.command_execution_state;
    job.structured_execution = snapshot.context.structured_execution.clone();
    job.validation_progress = snapshot.validation_progress.clone();
    job.activity = snapshot.activity;
    job.validation = snapshot.context.validation.clone();
    replace_log_from_snapshot(&mut job.stdout, &snapshot.stdout);
    replace_log_from_snapshot(&mut job.stderr, &snapshot.stderr);
    job.last_update_seq = snapshot.update_seq;
    job.recovery.phase = Some(JobRecoveryPhase::Reconciled);
    job.recovery.reconciled_at = Some(now);
    job.recovery.reason = Some(recovery_reason);
    job.recovery.recovering_since = None;
    notify_job_update(job);
}

fn remove_cleanup_terminal_jobs_locked(inner: &mut RunnerRegistryInner) {
    let removable = inner
        .jobs_by_id
        .iter()
        .filter(|(_, job)| {
            job.visibility == ShellJobVisibility::CleanupPending && job.lifecycle.is_terminal()
        })
        .map(|(job_id, _)| job_id.clone())
        .collect::<Vec<_>>();
    for job_id in removable {
        inner.jobs_by_id.remove(&job_id);
    }
}

fn prune_projected_structured_terminal_suppressions_locked(
    inner: &mut RunnerRegistryInner,
    now: i64,
) {
    for runner in inner.runners.values_mut() {
        runner.prune_projected_structured_terminal_suppressions(now);
    }
}

impl RunnerRegistry {
    fn prune_expired_terminal_jobs_locked(
        &self,
        inner: &mut RunnerRegistryInner,
        now: i64,
    ) -> usize {
        enum TerminalSweepAction {
            Observe(String),
            Remove(String, String, Option<String>),
        }

        let actions = inner
            .jobs_by_id
            .iter()
            .filter_map(|(job_id, job)| {
                if job.visibility != ShellJobVisibility::Public || !job.lifecycle.is_terminal() {
                    return None;
                }
                match job.observation.terminal_observed_at {
                    None => Some(TerminalSweepAction::Observe(job_id.clone())),
                    Some(observed_at)
                        if now.saturating_sub(observed_at) >= JOB_TERMINAL_RETENTION_SECS =>
                    {
                        Some(TerminalSweepAction::Remove(
                            job_id.clone(),
                            job.client_id.clone(),
                            job.request_id.clone(),
                        ))
                    }
                    Some(_) => None,
                }
            })
            .take(RECOVERY_SWEEP_PASS_CAP)
            .collect::<Vec<_>>();
        if actions.is_empty() {
            return 0;
        }
        let mut expired = Vec::new();
        for action in actions {
            match action {
                TerminalSweepAction::Observe(job_id) => {
                    if let Some(job) = inner.jobs_by_id.get_mut(&job_id) {
                        observe_job_terminal(job, now);
                    }
                }
                TerminalSweepAction::Remove(job_id, client_id, request_id) => {
                    expired.push((job_id, client_id, request_id));
                }
            }
        }
        if expired.is_empty() {
            return 0;
        }
        let expired_job_ids = expired
            .iter()
            .map(|(job_id, _, _)| job_id.clone())
            .collect::<HashSet<_>>();
        let distinct_runners = expired
            .iter()
            .map(|(_, client_id, _)| client_id.clone())
            .collect::<HashSet<_>>();
        for (_, client_id, request_id) in &expired {
            remove_job_request_mapping(inner, client_id, request_id.as_deref());
        }
        for client_id in distinct_runners {
            remove_job_control_requests(inner, &client_id, &expired_job_ids);
        }
        inner
            .request_to_job
            .retain(|_, job_id| !expired_job_ids.contains(job_id));
        for job_id in &expired_job_ids {
            inner.jobs_by_id.remove(job_id);
        }
        self.cleanup_intents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|job_id, _| !expired_job_ids.contains(job_id));
        expired.len()
    }
}

/// Maximum number of recovering jobs transitioned to lost in a single
/// recovery-timeout sweep pass. Bounds worst-case registry-mutex hold time
/// when many jobs belonging to one or more disconnected runners exceed the
/// deadline together. Jobs beyond this cap stay `recovering` until the next
/// pass (at most one sweep interval later), which is acceptable and
/// self-correcting.
pub(super) const RECOVERY_SWEEP_PASS_CAP: usize = 64;

/// Transition `recovering` jobs whose recovery deadline has elapsed to
/// terminal `lost` with `runner_recovery_deadline_exceeded`, and clean up
/// their pending request / request-to-job mappings. Shared by the
/// inventory-reconciliation path (scoped to one runner) and the periodic
/// recovery-timeout sweep (all runners). Returns the number of jobs lost.
///
/// Callers must already hold `inner`. Performs only in-memory work; no disk,
/// network, or await. `mark_job_lost` is idempotent (terminal guard) and sets
/// `ended_at` only once, so a race between this and the on-demand
/// `refresh_job_status_locked` path is harmless — the second call is a no-op.
pub(super) fn expire_recovering_jobs_locked(
    inner: &mut RunnerRegistryInner,
    client_filter: Option<&str>,
    now: i64,
    pass_cap: usize,
) -> usize {
    let grace = job_recovery_grace_secs();
    let expired = inner
        .jobs_by_id
        .iter()
        .filter_map(|(job_id, job)| {
            if !job.recovery_active() {
                return None;
            }
            if let Some(client_id) = client_filter {
                if job.client_id != client_id {
                    return None;
                }
            }
            let expired = job
                .recovery
                .recovering_since
                .is_some_and(|since| now.saturating_sub(since) >= grace);
            expired.then(|| {
                (
                    job_id.clone(),
                    job.client_id.clone(),
                    job.request_id.clone(),
                )
            })
        })
        .take(pass_cap)
        .collect::<Vec<_>>();
    if expired.is_empty() {
        return 0;
    }
    let expired_job_ids = expired
        .iter()
        .map(|(job_id, _, _)| job_id.clone())
        .collect::<HashSet<_>>();
    let distinct_runners = expired
        .iter()
        .map(|(_, client_id, _)| client_id.clone())
        .collect::<HashSet<_>>();
    for (job_id, _, _) in &expired {
        if let Some(job) = inner.jobs_by_id.get_mut(job_id) {
            // Re-check under the same lock: a concurrent reconciliation cannot
            // interleave (we hold the mutex for the whole call), but the filter
            // above ran on borrowed references; defend against the job having
            // already left `recovering` by any path.
            if job.recovery_active() {
                mark_job_lost(
                    job,
                    now,
                    JobRecoveryReason::RunnerRecoveryDeadlineExceeded,
                    "runner did not reconcile the job before the recovery deadline",
                );
            }
        }
    }
    for (job_id, client_id, request_id) in &expired {
        remove_job_request_mapping(inner, client_id, request_id.as_deref());
        let _ = job_id;
    }
    for client_id in distinct_runners {
        remove_job_control_requests(inner, &client_id, &expired_job_ids);
    }
    remove_cleanup_terminal_jobs_locked(inner);
    expired.len()
}

/// Periodic recovery-deadline sweep. NOT request-triggered: scans every
/// `recovering` job across all runners and transitions any whose grace window
/// has elapsed to `lost`. This closes the gap where a reconciliation-capable
/// runner disconnects permanently and nobody queries the job, which would
/// otherwise leave it in `recovering` forever. Pure in-memory: holds the
/// registry mutex only for bounded HashMap work (capped by
/// [`RECOVERY_SWEEP_PASS_CAP`]) and never awaits under it.
pub async fn recovery_timeout_sweep(registry: &RunnerRegistry) {
    registry.process_hidden_cleanup_intents().await;
    let now = crate::registry::now_ts();
    let mut inner = registry.inner.lock().await;
    registry.prune_expired_shared_key_runners_locked(&mut inner, now);
    prune_projected_structured_terminal_suppressions_locked(&mut inner, now);
    expire_recovering_jobs_locked(&mut inner, None, now, RECOVERY_SWEEP_PASS_CAP);
    registry.prune_expired_terminal_jobs_locked(&mut inner, now);
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReconciliationSummary {
    pub(super) inventory_active: usize,
    pub(super) inventory_terminal: usize,
    pub(super) reconstructed: usize,
    pub(super) updated: usize,
    pub(super) missing: usize,
    pub(super) suppressed_terminal: usize,
}

pub(super) fn reconcile_inventory_locked(
    inner: &mut RunnerRegistryInner,
    client_id: &str,
    runner_instance_id: &str,
    auth_group: Option<RunnerAccessGroup>,
    observation_epoch: std::sync::Arc<str>,
    inventory: &ShellJobInventory,
    now: i64,
) -> ReconciliationSummary {
    let mut summary = ReconciliationSummary {
        inventory_active: inventory
            .jobs
            .iter()
            .filter(|snapshot| is_runner_active_job_status(&snapshot.status))
            .count(),
        inventory_terminal: inventory
            .jobs
            .iter()
            .filter(|snapshot| is_final_job_status(&snapshot.status))
            .count(),
        ..ReconciliationSummary::default()
    };
    prune_projected_structured_terminal_suppressions_locked(inner, now);
    expire_recovering_jobs_locked(inner, Some(client_id), now, RECOVERY_SWEEP_PASS_CAP);

    let inventory_ids = inventory
        .jobs
        .iter()
        .map(|snapshot| snapshot.job_id.as_str())
        .collect::<HashSet<_>>();
    let missing = inner
        .jobs_by_id
        .iter()
        .filter(|(job_id, job)| {
            job.client_id == client_id
                && job.runner_instance_id == runner_instance_id
                && job.lifecycle.is_runner_active()
                && !inventory_ids.contains(job_id.as_str())
        })
        .map(|(job_id, job)| (job_id.clone(), job.request_id.clone()))
        .collect::<Vec<_>>();
    let missing_job_ids = missing
        .iter()
        .map(|(job_id, _)| job_id.clone())
        .collect::<HashSet<_>>();
    summary.missing = missing.len();
    for (job_id, request_id) in missing {
        if let Some(job) = inner.jobs_by_id.get_mut(&job_id) {
            mark_job_lost(
                job,
                now,
                JobRecoveryReason::RunnerInventoryMissing,
                "runner complete active inventory did not contain this job",
            );
        }
        remove_job_request_mapping(inner, client_id, request_id.as_deref());
    }
    remove_job_control_requests(inner, client_id, &missing_job_ids);

    for snapshot in &inventory.jobs {
        let suppress_unknown_terminal = is_final_job_status(&snapshot.status)
            && inner.runners.get(client_id).is_some_and(|runner| {
                runner.suppresses_projected_structured_terminal(
                    client_id,
                    runner_instance_id,
                    &snapshot.job_id,
                    &snapshot.request_id,
                    now,
                )
            });
        if let Some(existing) = inner.jobs_by_id.get_mut(&snapshot.job_id) {
            if existing.lifecycle.is_terminal() {
                // A server-authoritative terminal state (deadline, replacement,
                // or a previously accepted terminal result) never revives or
                // changes terminal class.
                continue;
            }
            let detached_instance_transfer = existing.runner_instance_id != runner_instance_id
                && detached_instance_transfer_allowed(existing, snapshot);
            if snapshot.update_seq < existing.last_update_seq {
                continue;
            }
            if snapshot.update_seq == existing.last_update_seq
                && !existing.recovery_active()
                && !detached_instance_transfer
            {
                continue;
            }
            if detached_instance_transfer {
                existing.runner_instance_id = runner_instance_id.to_string();
            }
            let recovery_reason = if detached_instance_transfer {
                JobRecoveryReason::DetachedInstanceTransfer
            } else {
                JobRecoveryReason::SameInstanceReconciliation
            };
            apply_snapshot(existing, snapshot, now, recovery_reason);
            summary.updated += 1;
        } else if suppress_unknown_terminal {
            summary.suppressed_terminal += 1;
            continue;
        } else {
            let mut record = record_from_snapshot(
                client_id,
                runner_instance_id,
                auth_group.clone(),
                observation_epoch.clone(),
                snapshot,
                now,
            );
            replace_log_from_snapshot(&mut record.stdout, &snapshot.stdout);
            replace_log_from_snapshot(&mut record.stderr, &snapshot.stderr);
            notify_job_update(&record);
            inner.jobs_by_id.insert(snapshot.job_id.clone(), record);
            summary.reconstructed += 1;
        }
        if is_final_job_status(&snapshot.status) {
            remove_job_request_mapping(inner, client_id, Some(&snapshot.request_id));
        }
    }
    remove_cleanup_terminal_jobs_locked(inner);
    summary
}

pub(super) fn terminate_instance_jobs_locked(
    inner: &mut RunnerRegistryInner,
    client_id: &str,
    runner_instance_id: &str,
    replacement_inventory: Option<&ShellJobInventory>,
    now: i64,
) {
    let jobs = inner
        .jobs_by_id
        .iter()
        .filter(|(_, job)| {
            let detached_instance_transfer = replacement_inventory.is_some_and(|inventory| {
                inventory.jobs.iter().any(|snapshot| {
                    snapshot.job_id == job.job_id
                        && detached_instance_transfer_allowed(job, snapshot)
                })
            });
            job.client_id == client_id
                && job.runner_instance_id == runner_instance_id
                && (job.lifecycle == JobLifecycleState::Queued || job.lifecycle.is_runner_active())
                && !detached_instance_transfer
        })
        .map(|(job_id, job)| (job_id.clone(), job.request_id.clone()))
        .collect::<Vec<_>>();
    let terminated_job_ids = jobs
        .iter()
        .map(|(job_id, _)| job_id.clone())
        .collect::<HashSet<_>>();
    for (job_id, request_id) in jobs {
        if let Some(job) = inner.jobs_by_id.get_mut(&job_id) {
            mark_job_lost(
                job,
                now,
                JobRecoveryReason::RunnerInstanceReplaced,
                "runner instance was replaced before the job completed or reconciled",
            );
        }
        remove_job_request_mapping(inner, client_id, request_id.as_deref());
    }
    remove_job_control_requests(inner, client_id, &terminated_job_ids);
    remove_cleanup_terminal_jobs_locked(inner);
}
