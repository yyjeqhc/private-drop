use super::get_projects;
use super::require_active_goal;
use super::types::{CommandRequestOpRequest, CommandRequestOpResponse, CommandResponse};
use super::{approve_command_request_inner, reject_command_request_inner};
use crate::get_db;
use crate::projects::ProjectConfig;
use crate::{CodexGoalRecord, CommandAuditRecord};
use salvo::prelude::*;

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

pub(super) fn op_response(
    op: &str,
    success: bool,
    records: Vec<CommandAuditRecord>,
    error: Option<String>,
) -> CommandRequestOpResponse {
    op_response_with_goals(op, success, records, Vec::new(), error)
}

pub(super) fn op_response_with_goals(
    op: &str,
    success: bool,
    records: Vec<CommandAuditRecord>,
    goals: Vec<CodexGoalRecord>,
    error: Option<String>,
) -> CommandRequestOpResponse {
    CommandRequestOpResponse {
        success,
        op: op.to_string(),
        request_id: records.first().map(|r| r.id.clone()),
        record: records.first().cloned(),
        goal_id: goals.first().map(|g| g.id.clone()),
        goal: goals.first().cloned(),
        records,
        goals,
        error,
    }
}

pub(super) fn build_goal_record(
    project: String,
    title: String,
    summary: Option<String>,
    now: i64,
    ttl_secs: i64,
) -> CodexGoalRecord {
    CodexGoalRecord {
        id: uuid::Uuid::new_v4().to_string(),
        project,
        title,
        summary,
        status: "pending".to_string(),
        created_at: now,
        expires_at: now + ttl_secs,
        closed_at: None,
        error: None,
    }
}

#[handler]
pub async fn codex_command_request_op(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(op_response(
            "unknown",
            false,
            Vec::new(),
            Some("Projects not configured".to_string()),
        )));
        return;
    };
    let Some(db) = get_db(depot) else {
        res.render(Json(op_response(
            "unknown",
            false,
            Vec::new(),
            Some("Database not configured".to_string()),
        )));
        return;
    };
    let body: CommandRequestOpRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(op_response(
                "unknown",
                false,
                Vec::new(),
                Some(format!("Invalid JSON: {}", e)),
            )));
            return;
        }
    };
    match body.op.as_str() {
        "create_goal" => {
            let Some(project) = body.project else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("project is required".to_string()),
                )));
                return;
            };
            let title = body.title.unwrap_or_else(|| "Development goal".to_string());
            if let Err(e) = validate_goal_text(&title, &body.summary) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            let ttl_secs = match validate_goal_ttl(body.ttl_secs) {
                Ok(ttl) => ttl,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            if let Err(e) = projects.get_project(&project) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            let now = chrono::Utc::now().timestamp();
            let goal = build_goal_record(project, title, body.summary, now, ttl_secs);
            if let Err(e) = db.insert_goal(&goal) {
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to create goal: {}", e)),
                )));
                return;
            }
            res.render(Json(op_response_with_goals(
                &body.op,
                true,
                Vec::new(),
                vec![goal],
                None,
            )));
        }
        "list_goals" => {
            if let Some(status) = &body.status {
                if let Err(e) = validate_goal_status(status) {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            }
            match db.list_goals(body.project.as_deref(), body.status.as_deref(), body.limit) {
                Ok(goals) => res.render(Json(op_response_with_goals(
                    &body.op,
                    true,
                    Vec::new(),
                    goals,
                    None,
                ))),
                Err(e) => res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to list goals: {}", e)),
                ))),
            }
        }
        "close_goal" => {
            let Some(goal_id) = body.goal_id else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("goal_id is required".to_string()),
                )));
                return;
            };
            match db.update_goal_status(
                &goal_id,
                "closed",
                chrono::Utc::now().timestamp(),
                body.reason.as_deref(),
            ) {
                Ok(Some(goal)) => res.render(Json(op_response_with_goals(
                    &body.op,
                    true,
                    Vec::new(),
                    vec![goal],
                    None,
                ))),
                Ok(None) => {
                    res.status_code(StatusCode::NOT_FOUND);
                    res.render(Json(op_response(
                        &body.op,
                        false,
                        Vec::new(),
                        Some("Goal not found".to_string()),
                    )));
                }
                Err(e) => res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to close goal: {}", e)),
                ))),
            }
        }
        "approve_goal" => {
            let Some(goal_id) = body.goal_id else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("goal_id is required".to_string()),
                )));
                return;
            };
            let now = chrono::Utc::now().timestamp();
            let current = match db.get_goal(&goal_id) {
                Ok(Some(goal)) => goal,
                Ok(None) => {
                    res.status_code(StatusCode::NOT_FOUND);
                    res.render(Json(op_response(
                        &body.op,
                        false,
                        Vec::new(),
                        Some("Goal not found".to_string()),
                    )));
                    return;
                }
                Err(e) => {
                    res.render(Json(op_response(
                        &body.op,
                        false,
                        Vec::new(),
                        Some(format!("Failed to load goal: {}", e)),
                    )));
                    return;
                }
            };
            if current.status != "pending" {
                res.render(Json(op_response_with_goals(
                    &body.op,
                    false,
                    Vec::new(),
                    vec![current],
                    Some("Goal is not pending".to_string()),
                )));
                return;
            }
            if current.expires_at < now {
                let expired = db
                    .update_pending_goal_status(
                        &goal_id,
                        "expired",
                        Some(now),
                        Some("Goal expired"),
                    )
                    .ok()
                    .flatten()
                    .unwrap_or(current);
                res.render(Json(op_response_with_goals(
                    &body.op,
                    false,
                    Vec::new(),
                    vec![expired],
                    Some("Goal expired".to_string()),
                )));
                return;
            }
            match db.update_pending_goal_status(&goal_id, "active", None, None) {
                Ok(Some(goal)) => res.render(Json(op_response_with_goals(
                    &body.op,
                    true,
                    Vec::new(),
                    vec![goal],
                    None,
                ))),
                Ok(None) => match db.get_goal(&goal_id) {
                    Ok(Some(goal)) => res.render(Json(op_response_with_goals(
                        &body.op,
                        false,
                        Vec::new(),
                        vec![goal],
                        Some("Goal is not pending".to_string()),
                    ))),
                    Ok(None) => {
                        res.status_code(StatusCode::NOT_FOUND);
                        res.render(Json(op_response(
                            &body.op,
                            false,
                            Vec::new(),
                            Some("Goal not found".to_string()),
                        )));
                    }
                    Err(e) => res.render(Json(op_response(
                        &body.op,
                        false,
                        Vec::new(),
                        Some(format!("Failed to load goal: {}", e)),
                    ))),
                },
                Err(e) => res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to approve goal: {}", e)),
                ))),
            }
        }
        "reject_goal" => {
            let Some(goal_id) = body.goal_id else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("goal_id is required".to_string()),
                )));
                return;
            };
            let now = chrono::Utc::now().timestamp();
            let reason = body.reason.as_deref().unwrap_or("Goal rejected");
            match db.update_pending_goal_status(&goal_id, "rejected", Some(now), Some(reason)) {
                Ok(Some(goal)) => res.render(Json(op_response_with_goals(
                    &body.op,
                    true,
                    Vec::new(),
                    vec![goal],
                    None,
                ))),
                Ok(None) => match db.get_goal(&goal_id) {
                    Ok(Some(goal)) => res.render(Json(op_response_with_goals(
                        &body.op,
                        false,
                        Vec::new(),
                        vec![goal],
                        Some("Goal is not pending".to_string()),
                    ))),
                    Ok(None) => {
                        res.status_code(StatusCode::NOT_FOUND);
                        res.render(Json(op_response(
                            &body.op,
                            false,
                            Vec::new(),
                            Some("Goal not found".to_string()),
                        )));
                    }
                    Err(e) => res.render(Json(op_response(
                        &body.op,
                        false,
                        Vec::new(),
                        Some(format!("Failed to load goal: {}", e)),
                    ))),
                },
                Err(e) => res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to reject goal: {}", e)),
                ))),
            }
        }
        "create_raw_and_approve" => {
            let Some(project) = body.project else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("project is required".to_string()),
                )));
                return;
            };
            let Some(goal_id) = body.goal_id else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("goal_id is required".to_string()),
                )));
                return;
            };
            let Some(command_text) = body.command_text else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("command_text is required".to_string()),
                )));
                return;
            };
            let goal = match require_active_goal(&db, &goal_id, &project) {
                Ok(goal) => goal,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            if let Err(e) = validate_command_request_reason(&body.reason) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            if let Err(e) = validate_raw_command_text(&command_text) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            let proj = match projects.get_project(&project) {
                Ok(p) => p,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            if !proj.allow_raw_command_requests {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("Raw command requests are not enabled for this project".to_string()),
                )));
                return;
            }
            let reason = Some(format!(
                "[goal:{}] {}",
                goal.id,
                body.reason.unwrap_or_else(|| goal.title.clone())
            ));
            let record = build_command_audit_record(
                project,
                "raw".to_string(),
                command_text.trim().to_string(),
                reason,
                chrono::Utc::now().timestamp(),
            );
            let request_id = record.id.clone();
            if let Err(e) = db.insert_command_request(&record) {
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to create raw command request: {}", e)),
                )));
                return;
            }
            let resp = approve_command_request_inner(&projects, &db, request_id);
            let records = resp.record.clone().into_iter().collect::<Vec<_>>();
            res.render(Json(CommandRequestOpResponse {
                success: resp.success,
                op: body.op,
                records,
                goals: vec![goal.clone()],
                request_id: resp.request_id,
                record: resp.record,
                goal_id: Some(goal.id.clone()),
                goal: Some(goal),
                error: resp.error,
            }));
        }
        "create_and_approve" => {
            let Some(project) = body.project else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("project is required".to_string()),
                )));
                return;
            };
            let Some(goal_id) = body.goal_id else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("goal_id is required".to_string()),
                )));
                return;
            };
            let Some(command) = body.command else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("command is required".to_string()),
                )));
                return;
            };
            let goal = match require_active_goal(&db, &goal_id, &project) {
                Ok(goal) => goal,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            if let Err(e) = validate_command_request_reason(&body.reason) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            let proj = match projects.get_project(&project) {
                Ok(p) => p,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            if !proj.allow_command_requests {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("Command requests are not enabled for this project".to_string()),
                )));
                return;
            }
            let command_text = match get_project_command(proj, &command) {
                Ok(cmd) => cmd,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            let reason = Some(format!(
                "[goal:{}] {}",
                goal.id,
                body.reason.unwrap_or_else(|| goal.title.clone())
            ));
            let record = build_command_audit_record(
                project,
                command,
                command_text,
                reason,
                chrono::Utc::now().timestamp(),
            );
            let request_id = record.id.clone();
            if let Err(e) = db.insert_command_request(&record) {
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to create command request: {}", e)),
                )));
                return;
            }
            let resp = approve_command_request_inner(&projects, &db, request_id);
            let records = resp.record.clone().into_iter().collect::<Vec<_>>();
            res.render(Json(CommandRequestOpResponse {
                success: resp.success,
                op: body.op,
                records,
                goals: vec![goal.clone()],
                request_id: resp.request_id,
                record: resp.record,
                goal_id: Some(goal.id.clone()),
                goal: Some(goal),
                error: resp.error,
            }));
        }
        "list" => {
            if let Some(status) = &body.status {
                if let Err(e) = validate_command_request_status(status) {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            }
            match db.list_command_requests(
                body.project.as_deref(),
                body.status.as_deref(),
                body.limit,
            ) {
                Ok(records) => res.render(Json(op_response(&body.op, true, records, None))),
                Err(e) => res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to list command requests: {}", e)),
                ))),
            }
        }
        "create" => {
            let Some(project) = body.project else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("project is required".to_string()),
                )));
                return;
            };
            let Some(command) = body.command else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("command is required".to_string()),
                )));
                return;
            };
            if let Err(e) = validate_command_request_reason(&body.reason) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            let proj = match projects.get_project(&project) {
                Ok(p) => p,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            if !proj.allow_command_requests {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("Command requests are not enabled for this project".to_string()),
                )));
                return;
            }
            let command_text = match get_project_command(proj, &command) {
                Ok(cmd) => cmd,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            let record = build_command_audit_record(
                project,
                command,
                command_text,
                body.reason,
                chrono::Utc::now().timestamp(),
            );
            if let Err(e) = db.insert_command_request(&record) {
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to create command request: {}", e)),
                )));
                return;
            }
            res.render(Json(op_response(&body.op, true, vec![record], None)));
        }
        "create_raw" => {
            let Some(project) = body.project else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("project is required".to_string()),
                )));
                return;
            };
            let Some(command_text) = body.command_text else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("command_text is required".to_string()),
                )));
                return;
            };
            if let Err(e) = validate_command_request_reason(&body.reason) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            if let Err(e) = validate_raw_command_text(&command_text) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            let proj = match projects.get_project(&project) {
                Ok(p) => p,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            if !proj.allow_raw_command_requests {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("Raw command requests are not enabled for this project".to_string()),
                )));
                return;
            }
            let record = build_command_audit_record(
                project,
                "raw".to_string(),
                command_text.trim().to_string(),
                body.reason,
                chrono::Utc::now().timestamp(),
            );
            if let Err(e) = db.insert_command_request(&record) {
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to create raw command request: {}", e)),
                )));
                return;
            }
            res.render(Json(op_response(&body.op, true, vec![record], None)));
        }
        "create_batch" => {
            let Some(project) = body.project else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("project is required".to_string()),
                )));
                return;
            };
            if body.requests.is_empty() || body.requests.len() > MAX_COMMAND_REQUEST_BATCH {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!(
                        "requests must contain 1-{} items",
                        MAX_COMMAND_REQUEST_BATCH
                    )),
                )));
                return;
            }
            let proj = match projects.get_project(&project) {
                Ok(p) => p,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            if !proj.allow_command_requests {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("Command requests are not enabled for this project".to_string()),
                )));
                return;
            }
            let now = chrono::Utc::now().timestamp();
            let mut records = Vec::with_capacity(body.requests.len());
            for item in body.requests {
                if let Err(e) = validate_command_request_reason(&item.reason) {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
                let command_text = match get_project_command(proj, &item.command) {
                    Ok(cmd) => cmd,
                    Err(e) => {
                        res.status_code(StatusCode::BAD_REQUEST);
                        res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                        return;
                    }
                };
                records.push(build_command_audit_record(
                    project.clone(),
                    item.command,
                    command_text,
                    item.reason,
                    now,
                ));
            }
            for record in &records {
                if let Err(e) = db.insert_command_request(record) {
                    res.render(Json(op_response(
                        &body.op,
                        false,
                        Vec::new(),
                        Some(format!("Failed to create command request: {}", e)),
                    )));
                    return;
                }
            }
            res.render(Json(op_response(&body.op, true, records, None)));
        }
        "approve" | "reject" => {
            let Some(request_id) = body.request_id else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("request_id is required".to_string()),
                )));
                return;
            };
            let resp = if body.op == "approve" {
                approve_command_request_inner(&projects, &db, request_id)
            } else {
                reject_command_request_inner(&db, request_id, body.reason)
            };
            let records = resp.record.clone().into_iter().collect::<Vec<_>>();
            res.render(Json(CommandRequestOpResponse {
                success: resp.success,
                op: body.op,
                records,
                goals: Vec::new(),
                request_id: resp.request_id,
                record: resp.record,
                goal_id: None,
                goal: None,
                error: resp.error,
            }));
        }
        "approve_batch" | "reject_batch" => {
            if body.request_ids.is_empty() || body.request_ids.len() > MAX_COMMAND_REQUEST_BATCH {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!(
                        "request_ids must contain 1-{} items",
                        MAX_COMMAND_REQUEST_BATCH
                    )),
                )));
                return;
            }
            let mut records = Vec::new();
            let mut all_success = true;
            let mut first_error = None;
            for request_id in body.request_ids {
                let resp = if body.op == "approve_batch" {
                    approve_command_request_inner(&projects, &db, request_id)
                } else {
                    reject_command_request_inner(&db, request_id, body.reason.clone())
                };
                all_success &= resp.success;
                if first_error.is_none() {
                    first_error = resp.error.clone();
                }
                if let Some(record) = resp.record {
                    records.push(record);
                }
            }
            res.render(Json(op_response(
                &body.op,
                all_success,
                records,
                first_error,
            )));
        }
        _ => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(op_response(
                &body.op,
                false,
                Vec::new(),
                Some("unsupported op".to_string()),
            )));
        }
    }
}
