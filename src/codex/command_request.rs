use crate::projects::ProjectConfig;
use crate::CommandAuditRecord;

use super::types::CommandResponse;

pub(super) const MAX_COMMAND_REASON_LEN: usize = 2_000;
pub(super) const MAX_RAW_COMMAND_LEN: usize = 2_000;
pub(super) const MAX_GOAL_TITLE_LEN: usize = 200;
pub(super) const MAX_GOAL_SUMMARY_LEN: usize = 4_000;
pub(super) const DEFAULT_GOAL_TTL_SECS: i64 = 2 * 60 * 60;
pub(super) const MAX_GOAL_TTL_SECS: i64 = 8 * 60 * 60;
pub(super) const MAX_COMMAND_REQUEST_BATCH: usize = 20;
pub(super) const COMMAND_REQUEST_TTL_SECS: i64 = 2 * 60 * 60;

pub(super) fn validate_goal_text(title: &str, summary: &Option<String>) -> Result<(), String> {
    let len = title.chars().count();
    if len == 0 {
        return Err("goal title cannot be empty".to_string());
    }
    if len > MAX_GOAL_TITLE_LEN {
        return Err(format!(
            "goal title is too long; maximum is {} characters",
            MAX_GOAL_TITLE_LEN
        ));
    }
    if let Some(summary) = summary {
        if summary.chars().count() > MAX_GOAL_SUMMARY_LEN {
            return Err(format!(
                "goal summary is too long; maximum is {} characters",
                MAX_GOAL_SUMMARY_LEN
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_goal_status(status: &str) -> Result<(), String> {
    match status {
        "pending" | "active" | "closed" | "expired" | "rejected" => Ok(()),
        _ => Err("invalid goal status filter".to_string()),
    }
}

pub(super) fn validate_goal_ttl(ttl_secs: Option<i64>) -> Result<i64, String> {
    let ttl = ttl_secs.unwrap_or(DEFAULT_GOAL_TTL_SECS);
    if !(60..=MAX_GOAL_TTL_SECS).contains(&ttl) {
        return Err(format!(
            "goal ttl_secs must be between 60 and {}",
            MAX_GOAL_TTL_SECS
        ));
    }
    Ok(ttl)
}

pub(super) fn validate_raw_command_text(command_text: &str) -> Result<(), String> {
    let trimmed = command_text.trim();
    if trimmed.is_empty() {
        return Err("raw command cannot be empty".to_string());
    }
    if command_text.chars().count() > MAX_RAW_COMMAND_LEN {
        return Err(format!(
            "raw command is too long; maximum is {} characters",
            MAX_RAW_COMMAND_LEN
        ));
    }
    if command_text
        .chars()
        .any(|ch| ch == '\0' || ch == '\r' || ch == '\n')
    {
        return Err("raw command must be a single line and cannot contain NUL".to_string());
    }
    let lower = trimmed.to_ascii_lowercase();
    let blocked_tokens = [
        "sudo",
        "su ",
        "apt",
        "apt-get",
        "systemctl",
        "service",
        "docker",
        "podman",
        "kubectl",
        "iptables",
        "ufw",
        "mkfs",
        "mount",
        "umount",
        "chmod -r",
        "chown -r",
        "rm -rf",
        "git push",
        "git fetch",
        "git pull",
        "git checkout",
        "git restore",
        "git clean",
        "git reset",
        "git submodule",
        "curl ",
        "wget ",
        "scp ",
        "rsync ",
    ];
    if blocked_tokens.iter().any(|token| lower.contains(token)) {
        return Err("raw command contains a blocked high-risk token".to_string());
    }
    Ok(())
}

pub(super) fn validate_command_request_reason(reason: &Option<String>) -> Result<(), String> {
    if let Some(reason) = reason {
        if reason.chars().count() > MAX_COMMAND_REASON_LEN {
            return Err(format!(
                "reason is too long; maximum is {} characters",
                MAX_COMMAND_REASON_LEN
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_command_request_status(status: &str) -> Result<(), String> {
    match status {
        "pending" | "running" | "completed" | "failed" | "rejected" | "expired" => Ok(()),
        _ => Err("invalid status filter".to_string()),
    }
}

pub(super) fn validate_command_name(command: &str) -> Result<(), String> {
    if command.is_empty() {
        return Err("command cannot be empty".to_string());
    }
    if command.len() > 100 {
        return Err("command is too long; maximum is 100 characters".to_string());
    }
    if !command
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.')
    {
        return Err(
            "command may only contain ASCII letters, digits, underscore, dash, and dot".to_string(),
        );
    }
    Ok(())
}

pub(super) fn command_error(project: &str, command: &str, error: String) -> CommandResponse {
    CommandResponse {
        success: false,
        project: project.to_string(),
        command: command.to_string(),
        exit_code: None,
        duration_ms: 0,
        stdout_tail: None,
        stderr_tail: None,
        truncated: false,
        error: Some(error),
    }
}

pub(super) fn get_project_command(proj: &ProjectConfig, command: &str) -> Result<String, String> {
    validate_command_name(command)?;
    proj.commands.get(command).cloned().ok_or_else(|| {
        let mut commands = proj.commands.keys().cloned().collect::<Vec<_>>();
        commands.sort();
        format!(
            "Command '{}' is not configured. Available: {}",
            command,
            commands.join(", ")
        )
    })
}

pub(super) fn build_command_audit_record(
    project: String,
    command: String,
    command_text: String,
    reason: Option<String>,
    created_at: i64,
) -> CommandAuditRecord {
    CommandAuditRecord {
        id: uuid::Uuid::new_v4().to_string(),
        project,
        command,
        command_text: Some(command_text),
        reason,
        status: "pending".to_string(),
        created_at,
        approved_at: None,
        executed_at: None,
        exit_code: None,
        stdout_tail: None,
        stderr_tail: None,
        error: None,
    }
}
