//! Bounded historical Job evidence. This contract carries no execution authority.
use crate::runner_job_lifecycle::RunnerJobLifecycle;
use crate::runner_protocol::{ShellJobSnapshot, JOB_TERMINAL_RETENTION_SECS};

/// Non-secret isolation partition captured when a Runner or Job is admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerAccessGroup {
    /// SHA-256 shared-key/OAuth-bridge group, never the credential itself.
    SharedKey(String),
    ProjectGrant(String),
    OpenAnonymous,
}

/// Preserve the Server's retained streams, including its bounded truncation marker.
pub const JOB_RECEIPT_STREAM_MAX_BYTES: usize = 256 * 1024 + 128;
pub const JOB_RECEIPT_PAYLOAD_MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedJobReceipt {
    pub client_id: String,
    /// Historical provenance only; never a current Runner lease.
    pub runner_instance_id: String,
    pub auth_group: Option<RunnerAccessGroup>,
    /// None with no group is a proven unowned admission: visible only through
    /// the existing internal/global observation bypass, never ordinary owners.
    pub owner_at_admission: Option<String>,
    pub kind: String,
    pub snapshot: ShellJobSnapshot,
    pub terminal_observed_at: i64,
    pub expires_at: i64,
}

impl RetainedJobReceipt {
    /// Validate before writing and after reading. A malformed row is never a
    /// source of active state or an implicit anonymous/owner partition.
    pub fn validate(&self, now: i64) -> Result<(), &'static str> {
        let text = |s: &str, max: usize| !s.is_empty() && s.len() <= max && !s.contains('\0');
        let snapshot = &self.snapshot;
        if !text(&self.client_id, 128)
            || !text(&self.runner_instance_id, 128)
            || !text(&snapshot.job_id, 128)
            || !text(&snapshot.request_id, 128)
            || !text(&self.kind, 128)
            || !matches!(self.kind.as_str(), "shell" | "run_process" | "run_script")
            || self.terminal_observed_at <= 0
            || self.terminal_observed_at > now
            || self.expires_at
                != self
                    .terminal_observed_at
                    .saturating_add(JOB_TERMINAL_RETENTION_SECS)
            || self.expires_at <= now
        {
            return Err("invalid receipt identity or deadline");
        }
        match &self.auth_group {
            Some(RunnerAccessGroup::SharedKey(group))
                if group.len() == 64 && group.bytes().all(|b| b.is_ascii_hexdigit()) => {}
            Some(RunnerAccessGroup::ProjectGrant(group)) if text(group, 256) => {}
            Some(RunnerAccessGroup::OpenAnonymous) => {}
            None if self
                .owner_at_admission
                .as_deref()
                .is_none_or(|owner| text(owner, 256) && !owner.trim().is_empty()) => {}
            _ => return Err("invalid receipt authorization partition"),
        }
        if self
            .owner_at_admission
            .as_deref()
            .is_some_and(|owner| !text(owner, 256))
        {
            return Err("invalid receipt owner");
        }
        if !RunnerJobLifecycle::from_wire(&snapshot.status).is_ok_and(|state| state.is_terminal())
            || snapshot.ended_at.is_none()
            || snapshot.created_at <= 0
            || (snapshot.status == "completed" && snapshot.exit_code != Some(0))
            || snapshot.activity.is_some()
            // Validation identity may contain exact argv. Receipts deliberately
            // retain only step names/progress, never executable validation plans.
            || snapshot.context.validation.is_some()
            || snapshot.context.structured_execution.as_ref().is_some_and(|metadata| !metadata.is_valid() || metadata.execution_source == "run_detached_process")
        {
            return Err("invalid receipt terminal snapshot");
        }
        let context = &snapshot.context;
        if context.runtime_project_id.as_ref().is_some_and(|project| {
            project
                .strip_prefix(&format!("agent:{}:", self.client_id))
                .is_none_or(str::is_empty)
        }) || context
            .workflow_session_id
            .as_deref()
            .is_some_and(|session| {
                context.runtime_project_id.is_none()
                    || !crate::workflow_session_contract::is_valid_session_id(session)
            })
        {
            return Err("invalid receipt project/session context");
        }
        if crate::audit_preview::command_preview(&context.command_preview)
            != context.command_preview
            || context.command_preview.chars().count()
                > crate::audit_preview::COMMAND_PREVIEW_MAX_CHARS + 1
            || [
                context.runtime_project_id.as_deref(),
                context.workflow_session_id.as_deref(),
                context.ssh_resource.as_deref(),
                context.project_cwd.as_deref(),
                context.cwd.as_deref(),
                context.purpose.as_deref(),
                context.shell.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|value| !text(value, 4096))
            || snapshot
                .error
                .as_deref()
                .is_some_and(|value| value.len() > 16 * 1024)
            || context.validation_steps.len() > 3
            || context
                .validation_steps
                .iter()
                .any(|step| !matches!(step.as_str(), "format" | "check" | "test"))
            || snapshot
                .validation_progress
                .as_ref()
                .is_some_and(|progress| {
                    progress.completed > context.validation_steps.len()
                        || [
                            progress.current_step.as_ref(),
                            progress.failed_step.as_ref(),
                        ]
                        .into_iter()
                        .flatten()
                        .any(|step| !context.validation_steps.contains(step))
                })
        {
            return Err("invalid receipt context");
        }
        for stream in [&snapshot.stdout, &snapshot.stderr] {
            if stream.tail.len() > JOB_RECEIPT_STREAM_MAX_BYTES
                || stream.first_retained_line == 0
                || stream
                    .first_retained_line
                    .checked_add(stream.tail.lines().count())
                    != Some(stream.next_line)
                || (stream.first_retained_line > 1 && !stream.truncated)
            {
                return Err("invalid receipt log bounds");
            }
        }
        Ok(())
    }
}
