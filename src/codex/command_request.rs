use super::command_workflow::require_active_goal;
use super::command_workflow::{approve_command_request_inner, reject_command_request_inner};
use super::get_projects;
use super::jobs::build_script_job_command;
use super::shell::sanitize_tail;
use super::trusted::{
    build_trusted_result, build_trusted_wrapper, check_background_escape, check_denylist,
    check_secret_read, validate_response_mode, validate_trusted_reason, validate_trusted_script,
    validate_trusted_timeout, write_trusted_audit,
};
use super::types::{
    CheckRequest, CheckResponse, CommandApproveRequest, CommandRejectRequest, CommandRequest,
    CommandRequestBatchCreate, CommandRequestBatchResponse, CommandRequestCreate,
    CommandRequestOpRequest, CommandRequestOpResponse, CommandRequestResponse,
    CommandRequestsListRequest, CommandRequestsListResponse, CommandResponse,
    RawCommandRequestCreate,
};
use super::{run_project_cmd, CHECK_TIMEOUT_SECS, MAX_OUTPUT_LEN};
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

pub(super) fn build_raw_command_text_from_op_request(
    command_text: Option<String>,
    script_path: Option<String>,
    script_args: &[String],
) -> Result<String, String> {
    match (command_text, script_path) {
        (Some(_), Some(_)) => {
            Err("provide either command_text or script_path, not both".to_string())
        }
        (Some(command_text), None) => Ok(command_text),
        (None, Some(script_path)) => build_script_job_command(&script_path, script_args),
        (None, None) => Err("command_text or script_path is required".to_string()),
    }
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
        trusted_result: None,
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
        "create_goal" | "create_goal_and_approve" => {
            let approve_immediately = body.op == "create_goal_and_approve";
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
            let mut goal = build_goal_record(project, title, body.summary, now, ttl_secs);
            if approve_immediately {
                goal.status = "active".to_string();
            }
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
            let command_text = match build_raw_command_text_from_op_request(
                body.command_text,
                body.script_path,
                &body.script_args,
            ) {
                Ok(command_text) => command_text,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
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
                trusted_result: None,
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
                trusted_result: None,
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
            let command_text = match build_raw_command_text_from_op_request(
                body.command_text,
                body.script_path,
                &body.script_args,
            ) {
                Ok(command_text) => command_text,
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
                trusted_result: None,
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
        "create_trusted_raw" | "create_trusted_raw_and_approve" => {
            let approve_immediately = body.op == "create_trusted_raw_and_approve";
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
            // Require script_text or command_text
            let script = match (body.script_text.as_deref(), body.command_text.as_deref()) {
                (Some(script_text), None) => script_text.to_string(),
                (None, Some(cmd_text)) => cmd_text.to_string(),
                (Some(_), Some(_)) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(
                        &body.op,
                        false,
                        Vec::new(),
                        Some("provide either script_text or command_text, not both".to_string()),
                    )));
                    return;
                }
                (None, None) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(
                        &body.op,
                        false,
                        Vec::new(),
                        Some("script_text or command_text is required".to_string()),
                    )));
                    return;
                }
            };
            if let Err(e) = validate_trusted_script(&script) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            if let Err(e) = validate_trusted_reason(&body.reason) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            let timeout_secs = match validate_trusted_timeout(body.timeout_secs) {
                Ok(t) => t,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            let response_mode = match validate_response_mode(&body.response_mode) {
                Ok(m) => m,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            // For approve variant, require goal_id
            if approve_immediately {
                let Some(_goal_id) = body.goal_id.as_deref() else {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(
                        &body.op,
                        false,
                        Vec::new(),
                        Some("goal_id is required for create_trusted_raw_and_approve".to_string()),
                    )));
                    return;
                };
            }
            // Validate project exists and allows raw command requests
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
            // Safety checks
            if let Some(err) = check_denylist(&script) {
                // Log the blocked attempt
                let reason_text = body.reason.as_deref().unwrap_or("no reason");
                let _ = write_trusted_audit(
                    &proj.root().join(".codex").join("audit"),
                    &project,
                    &proj.path,
                    reason_text,
                    &script,
                    chrono::Utc::now().timestamp(),
                    chrono::Utc::now().timestamp(),
                    -1,
                    0,
                    false,
                    false,
                    true,
                );
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(CommandRequestOpResponse {
                    success: false,
                    op: body.op,
                    records: Vec::new(),
                    goals: Vec::new(),
                    request_id: None,
                    record: None,
                    goal_id: None,
                    goal: None,
                    error: Some(err),
                    trusted_result: None,
                }));
                return;
            }
            if let Some(err) = check_secret_read(&script) {
                let reason_text = body.reason.as_deref().unwrap_or("no reason");
                let _ = write_trusted_audit(
                    &proj.root().join(".codex").join("audit"),
                    &project,
                    &proj.path,
                    reason_text,
                    &script,
                    chrono::Utc::now().timestamp(),
                    chrono::Utc::now().timestamp(),
                    -1,
                    0,
                    false,
                    false,
                    true,
                );
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(CommandRequestOpResponse {
                    success: false,
                    op: body.op,
                    records: Vec::new(),
                    goals: Vec::new(),
                    request_id: None,
                    record: None,
                    goal_id: None,
                    goal: None,
                    error: Some(err),
                    trusted_result: None,
                }));
                return;
            }
            if let Some(err) = check_background_escape(&script) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(CommandRequestOpResponse {
                    success: false,
                    op: body.op,
                    records: Vec::new(),
                    goals: Vec::new(),
                    request_id: None,
                    record: None,
                    goal_id: None,
                    goal: None,
                    error: Some(err),
                    trusted_result: None,
                }));
                return;
            }
            // If not approving immediately, just create the record and return
            if !approve_immediately {
                let record = build_command_audit_record(
                    project.clone(),
                    "trusted_raw".to_string(),
                    script.trim().to_string(),
                    body.reason.clone(),
                    chrono::Utc::now().timestamp(),
                );
                if let Err(e) = db.insert_command_request(&record) {
                    res.render(Json(op_response(
                        &body.op,
                        false,
                        Vec::new(),
                        Some(format!(
                            "Failed to create trusted raw command request: {}",
                            e
                        )),
                    )));
                    return;
                }
                res.render(Json(CommandRequestOpResponse {
                    success: true,
                    op: body.op.clone(),
                    records: vec![record],
                    goals: Vec::new(),
                    request_id: None,
                    record: None,
                    goal_id: None,
                    goal: None,
                    error: None,
                    trusted_result: None,
                }));
                return;
            }
            // Execute immediately for create_trusted_raw_and_approve
            let wrapped = build_trusted_wrapper(&script);
            let start_time = chrono::Utc::now().timestamp();
            let (code, stdout, stderr, duration_ms) =
                run_project_cmd(proj, &wrapped, timeout_secs, projects.ssh.as_ref());
            let end_time = chrono::Utc::now().timestamp();
            // Write audit
            let audit_dir = proj.root().join(".codex").join("audit");
            let audit_log_path = format!(".codex/audit/trusted_{}.json", start_time);
            let reason_text = body.reason.as_deref().unwrap_or("no reason");
            let _ = write_trusted_audit(
                &audit_dir,
                &project,
                &proj.path,
                reason_text,
                &script,
                start_time,
                end_time,
                code,
                duration_ms,
                stdout.len()
                    > if response_mode == "full" {
                        40_000
                    } else {
                        8_000
                    },
                stderr.len()
                    > if response_mode == "full" {
                        20_000
                    } else {
                        4_000
                    },
                false,
            );
            let trusted_result = build_trusted_result(
                code,
                duration_ms,
                &proj.path,
                &stdout,
                &stderr,
                &response_mode,
                Some(audit_log_path),
                false,
            );
            // Also create an audit record in the DB
            let record = build_command_audit_record(
                project.clone(),
                "trusted_raw".to_string(),
                script.trim().to_string(),
                body.reason.clone(),
                start_time,
            );
            let request_id = record.id.clone();
            if let Err(e) = db.insert_command_request(&record) {
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!(
                        "Failed to create trusted raw command request: {}",
                        e
                    )),
                )));
                return;
            }
            let success = code == 0;
            tracing::info!(
                target: "codex.metrics",
                operation = "createTrustedRawAndApprove",
                project = %project,
                exit_code = code,
                duration_ms = duration_ms,
                timeout_secs = timeout_secs,
                response_mode = %response_mode,
                success = success,
                "codex_trusted_raw_executed"
            );
            res.render(Json(CommandRequestOpResponse {
                success,
                op: body.op.clone(),
                records: vec![record],
                goals: Vec::new(),
                request_id: Some(request_id),
                record: None,
                goal_id: body.goal_id.clone(),
                goal: None,
                error: if success {
                    None
                } else {
                    Some("trusted command failed".to_string())
                },
                trusted_result: Some(trusted_result),
            }));
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

#[handler]
pub async fn codex_command_request_raw(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Projects not configured".to_string()),
        }));
        return;
    };
    let Some(db) = get_db(depot) else {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Database not configured".to_string()),
        }));
        return;
    };
    let body: RawCommandRequestCreate = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: None,
                record: None,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    if let Err(e) = validate_command_request_reason(&body.reason) {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some(e),
        }));
        return;
    }
    if let Err(e) = validate_raw_command_text(&body.command_text) {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some(e),
        }));
        return;
    }
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: None,
                record: None,
                error: Some(e),
            }));
            return;
        }
    };
    if !proj.allow_raw_command_requests {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Raw command requests are not enabled for this project".to_string()),
        }));
        return;
    }
    let record = build_command_audit_record(
        body.project,
        "raw".to_string(),
        body.command_text.trim().to_string(),
        body.reason,
        chrono::Utc::now().timestamp(),
    );
    let request_id = record.id.clone();
    if let Err(e) = db.insert_command_request(&record) {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some(format!("Failed to create raw command request: {}", e)),
        }));
        return;
    }
    res.render(Json(CommandRequestResponse {
        success: true,
        request_id: Some(request_id),
        record: Some(record),
        error: None,
    }));
}

#[handler]
pub async fn codex_command_requests(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(db) = get_db(depot) else {
        res.render(Json(CommandRequestsListResponse {
            success: false,
            records: Vec::new(),
            error: Some("Database not configured".to_string()),
        }));
        return;
    };
    let body: CommandRequestsListRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestsListResponse {
                success: false,
                records: Vec::new(),
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    if let Some(status) = &body.status {
        if let Err(e) = validate_command_request_status(status) {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestsListResponse {
                success: false,
                records: Vec::new(),
                error: Some(e),
            }));
            return;
        }
    }
    match db.list_command_requests(body.project.as_deref(), body.status.as_deref(), body.limit) {
        Ok(records) => res.render(Json(CommandRequestsListResponse {
            success: true,
            records,
            error: None,
        })),
        Err(e) => res.render(Json(CommandRequestsListResponse {
            success: false,
            records: Vec::new(),
            error: Some(format!("Failed to list command requests: {}", e)),
        })),
    }
}

#[handler]
pub async fn codex_command_request_batch(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(CommandRequestBatchResponse {
            success: false,
            records: Vec::new(),
            error: Some("Projects not configured".to_string()),
        }));
        return;
    };
    let Some(db) = get_db(depot) else {
        res.render(Json(CommandRequestBatchResponse {
            success: false,
            records: Vec::new(),
            error: Some("Database not configured".to_string()),
        }));
        return;
    };
    let body: CommandRequestBatchCreate = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestBatchResponse {
                success: false,
                records: Vec::new(),
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    if body.requests.is_empty() || body.requests.len() > MAX_COMMAND_REQUEST_BATCH {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(CommandRequestBatchResponse {
            success: false,
            records: Vec::new(),
            error: Some(format!(
                "requests must contain 1-{} items",
                MAX_COMMAND_REQUEST_BATCH
            )),
        }));
        return;
    }
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestBatchResponse {
                success: false,
                records: Vec::new(),
                error: Some(e),
            }));
            return;
        }
    };
    if !proj.allow_command_requests {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(CommandRequestBatchResponse {
            success: false,
            records: Vec::new(),
            error: Some("Command requests are not enabled for this project".to_string()),
        }));
        return;
    }
    let now = chrono::Utc::now().timestamp();
    let mut records = Vec::with_capacity(body.requests.len());
    for item in body.requests {
        if let Err(e) = validate_command_request_reason(&item.reason) {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestBatchResponse {
                success: false,
                records: Vec::new(),
                error: Some(e),
            }));
            return;
        }
        let command_text = match get_project_command(proj, &item.command) {
            Ok(cmd) => cmd,
            Err(e) => {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(CommandRequestBatchResponse {
                    success: false,
                    records: Vec::new(),
                    error: Some(e),
                }));
                return;
            }
        };
        records.push(build_command_audit_record(
            body.project.clone(),
            item.command,
            command_text,
            item.reason,
            now,
        ));
    }
    for record in &records {
        if let Err(e) = db.insert_command_request(record) {
            res.render(Json(CommandRequestBatchResponse {
                success: false,
                records: Vec::new(),
                error: Some(format!("Failed to create command request: {}", e)),
            }));
            return;
        }
    }
    res.render(Json(CommandRequestBatchResponse {
        success: true,
        records,
        error: None,
    }));
}

#[handler]
pub async fn codex_command_reject(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(db) = get_db(depot) else {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Database not configured".to_string()),
        }));
        return;
    };
    let body: CommandRejectRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: None,
                record: None,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    if let Err(e) = validate_command_request_reason(&body.reason) {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: Some(body.request_id),
            record: None,
            error: Some(e),
        }));
        return;
    }
    let error = body
        .reason
        .unwrap_or_else(|| "Rejected by user".to_string());
    match db.reject_command_request(&body.request_id, chrono::Utc::now().timestamp(), &error) {
        Ok(Some(record)) => res.render(Json(CommandRequestResponse {
            success: true,
            request_id: Some(record.id.clone()),
            record: Some(record),
            error: None,
        })),
        Ok(None) => match db.get_command_request(&body.request_id) {
            Ok(Some(record)) => {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(CommandRequestResponse {
                    success: false,
                    request_id: Some(record.id.clone()),
                    record: Some(record),
                    error: Some("Command request is not pending".to_string()),
                }));
            }
            Ok(None) => {
                res.status_code(StatusCode::NOT_FOUND);
                res.render(Json(CommandRequestResponse {
                    success: false,
                    request_id: Some(body.request_id),
                    record: None,
                    error: Some("Command request not found".to_string()),
                }));
            }
            Err(e) => res.render(Json(CommandRequestResponse {
                success: false,
                request_id: Some(body.request_id),
                record: None,
                error: Some(format!("Failed to load command request: {}", e)),
            })),
        },
        Err(e) => res.render(Json(CommandRequestResponse {
            success: false,
            request_id: Some(body.request_id),
            record: None,
            error: Some(format!("Failed to reject command request: {}", e)),
        })),
    }
}

#[handler]
pub async fn codex_command_approve(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Projects not configured".to_string()),
        }));
        return;
    };
    let Some(db) = get_db(depot) else {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Database not configured".to_string()),
        }));
        return;
    };
    let body: CommandApproveRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: None,
                record: None,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    let approved_at = chrono::Utc::now().timestamp();
    let min_created_at = approved_at - COMMAND_REQUEST_TTL_SECS;
    let mut record =
        match db.claim_command_request_for_execution(&body.request_id, approved_at, min_created_at)
        {
            Ok(Some(record)) => record,
            Ok(None) => match db.get_command_request(&body.request_id) {
                Ok(Some(record)) => {
                    if record.status == "pending" && record.created_at < min_created_at {
                        let error = "Command request expired".to_string();
                        let expired = db
                            .expire_command_request(&record.id, approved_at, &error)
                            .ok()
                            .flatten()
                            .unwrap_or(record);
                        res.status_code(StatusCode::BAD_REQUEST);
                        res.render(Json(CommandRequestResponse {
                            success: false,
                            request_id: Some(expired.id.clone()),
                            record: Some(expired),
                            error: Some(error),
                        }));
                        return;
                    }
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(CommandRequestResponse {
                        success: false,
                        request_id: Some(record.id.clone()),
                        record: Some(record),
                        error: Some("Command request is not pending".to_string()),
                    }));
                    return;
                }
                Ok(None) => {
                    res.status_code(StatusCode::NOT_FOUND);
                    res.render(Json(CommandRequestResponse {
                        success: false,
                        request_id: Some(body.request_id),
                        record: None,
                        error: Some("Command request not found".to_string()),
                    }));
                    return;
                }
                Err(e) => {
                    res.render(Json(CommandRequestResponse {
                        success: false,
                        request_id: Some(body.request_id),
                        record: None,
                        error: Some(format!("Failed to load command request: {}", e)),
                    }));
                    return;
                }
            },
            Err(e) => {
                res.render(Json(CommandRequestResponse {
                    success: false,
                    request_id: Some(body.request_id),
                    record: None,
                    error: Some(format!("Failed to claim command request: {}", e)),
                }));
                return;
            }
        };
    let proj = match projects.get_project(&record.project) {
        Ok(p) => p,
        Err(e) => {
            record.status = "failed".to_string();
            record.executed_at = Some(chrono::Utc::now().timestamp());
            record.error = Some(e.clone());
            let _ = db.update_command_request_result(&record);
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: Some(record.id.clone()),
                record: Some(record),
                error: Some(e),
            }));
            return;
        }
    };
    if !proj.allow_command_requests {
        let error = "Command requests are not enabled for this project".to_string();
        record.status = "failed".to_string();
        record.executed_at = Some(chrono::Utc::now().timestamp());
        record.error = Some(error.clone());
        let _ = db.update_command_request_result(&record);
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: Some(record.id.clone()),
            record: Some(record),
            error: Some(error),
        }));
        return;
    }
    let cmd = match record.command_text.clone() {
        Some(cmd) if !cmd.is_empty() => cmd,
        _ => {
            let error = "Command request is missing command_text snapshot".to_string();
            record.status = "failed".to_string();
            record.executed_at = Some(chrono::Utc::now().timestamp());
            record.error = Some(error.clone());
            let _ = db.update_command_request_result(&record);
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: Some(record.id.clone()),
                record: Some(record),
                error: Some(error),
            }));
            return;
        }
    };
    let (code, stdout, stderr, duration_ms) =
        run_project_cmd(proj, &cmd, CHECK_TIMEOUT_SECS, projects.ssh.as_ref());
    let (stdout_tail, stdout_trunc) = sanitize_tail(&stdout, MAX_OUTPUT_LEN);
    let (stderr_tail, stderr_trunc) = sanitize_tail(&stderr, MAX_OUTPUT_LEN);
    let now = chrono::Utc::now().timestamp();
    record.status = if code == 0 { "completed" } else { "failed" }.to_string();
    record.approved_at = Some(approved_at);
    record.executed_at = Some(now);
    record.exit_code = Some(code);
    record.stdout_tail = Some(stdout_tail);
    record.stderr_tail = Some(stderr_tail);
    record.error = if code == 0 {
        None
    } else {
        Some("command failed".to_string())
    };
    if let Err(e) = db.update_command_request_result(&record) {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: Some(record.id.clone()),
            record: Some(record),
            error: Some(format!("Failed to update command request: {}", e)),
        }));
        return;
    }
    tracing::info!(
        target: "codex.metrics",
        operation = "approveCommandRequest",
        project = %record.project,
        command = %record.command,
        request_id = %record.id,
        success = code == 0,
        exit_code = code,
        duration_ms = duration_ms,
        truncated = stdout_trunc || stderr_trunc,
        "codex_command_request_executed"
    );
    res.render(Json(CommandRequestResponse {
        success: code == 0,
        request_id: Some(record.id.clone()),
        record: Some(record),
        error: if code == 0 {
            None
        } else {
            Some("command failed".to_string())
        },
    }));
}

#[handler]
pub async fn codex_command_request(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Projects not configured".to_string()),
        }));
        return;
    };
    let Some(db) = get_db(depot) else {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Database not configured".to_string()),
        }));
        return;
    };
    let body: CommandRequestCreate = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: None,
                record: None,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    if let Err(e) = validate_command_request_reason(&body.reason) {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some(e),
        }));
        return;
    }
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: None,
                record: None,
                error: Some(e),
            }));
            return;
        }
    };
    if !proj.allow_command_requests {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Command requests are not enabled for this project".to_string()),
        }));
        return;
    }
    let command_text = match get_project_command(proj, &body.command) {
        Ok(cmd) => cmd,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: None,
                record: None,
                error: Some(e),
            }));
            return;
        }
    };
    let now = chrono::Utc::now().timestamp();
    let record =
        build_command_audit_record(body.project, body.command, command_text, body.reason, now);
    let request_id = record.id.clone();
    if let Err(e) = db.insert_command_request(&record) {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some(format!("Failed to create command request: {}", e)),
        }));
        return;
    }
    tracing::info!(
        target: "codex.metrics",
        operation = "createCommandRequest",
        project = %record.project,
        command = %record.command,
        request_id = %request_id,
        "codex_command_request_created"
    );
    res.render(Json(CommandRequestResponse {
        success: true,
        request_id: Some(request_id),
        record: Some(record),
        error: None,
    }));
}

#[handler]
pub async fn codex_command(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(CommandResponse {
            success: false,
            project: String::new(),
            command: String::new(),
            exit_code: None,
            duration_ms: 0,
            stdout_tail: None,
            stderr_tail: None,
            truncated: false,
            error: Some("Projects not configured".to_string()),
        }));
        return;
    };
    let body: CommandRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandResponse {
                success: false,
                project: String::new(),
                command: String::new(),
                exit_code: None,
                duration_ms: 0,
                stdout_tail: None,
                stderr_tail: None,
                truncated: false,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(command_error(&body.project, &body.command, e)));
            return;
        }
    };
    let cmd = match get_project_command(proj, &body.command) {
        Ok(cmd) => cmd,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(command_error(&body.project, &body.command, e)));
            return;
        }
    };
    let (code, stdout, stderr, duration_ms) =
        run_project_cmd(proj, &cmd, CHECK_TIMEOUT_SECS, projects.ssh.as_ref());
    let (stdout_tail, stdout_trunc) = sanitize_tail(&stdout, MAX_OUTPUT_LEN);
    let (stderr_tail, stderr_trunc) = sanitize_tail(&stderr, MAX_OUTPUT_LEN);
    let truncated = stdout_trunc || stderr_trunc;
    let success = code == 0;
    tracing::info!(
        target: "codex.metrics",
        operation = "runProjectCommand",
        project = %body.project,
        command = %body.command,
        executor = if proj.is_ssh() { "ssh" } else { "local" },
        success = success,
        exit_code = code,
        duration_ms = duration_ms,
        ssh_calls = if proj.is_ssh() { 1 } else { 0 },
        control_master = projects.ssh.as_ref().map(|s| s.control_master).unwrap_or(false),
        "codex_command_completed"
    );
    res.render(Json(CommandResponse {
        success,
        project: body.project,
        command: body.command,
        exit_code: Some(code),
        duration_ms,
        stdout_tail: Some(stdout_tail),
        stderr_tail: Some(stderr_tail),
        truncated,
        error: if success {
            None
        } else {
            Some("command failed".to_string())
        },
    }));
}

#[handler]
pub async fn codex_check(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(CheckResponse {
            success: false,
            suite: None,
            exit_code: None,
            duration_ms: None,
            stdout_tail: None,
            stderr_tail: None,
            truncated: false,
            error: Some("Projects not configured".to_string()),
        }));
        return;
    };
    let body: CheckRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CheckResponse {
                success: false,
                suite: None,
                exit_code: None,
                duration_ms: None,
                stdout_tail: None,
                stderr_tail: None,
                truncated: false,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CheckResponse {
                success: false,
                suite: Some(body.suite),
                exit_code: None,
                duration_ms: None,
                stdout_tail: None,
                stderr_tail: None,
                truncated: false,
                error: Some(e),
            }));
            return;
        }
    };
    if !proj.is_check_allowed(&body.suite) {
        res.status_code(StatusCode::FORBIDDEN);
        let suite = body.suite.clone();
        res.render(Json(CheckResponse {
            success: false,
            suite: Some(body.suite),
            exit_code: None,
            duration_ms: None,
            stdout_tail: None,
            stderr_tail: None,
            truncated: false,
            error: Some(format!(
                "Check '{}' is not allowed. Allowed: {}",
                suite,
                proj.allowed_checks.join(", ")
            )),
        }));
        return;
    }
    let cmd = match proj.get_check_command(&body.suite) {
        Ok(c) => c,
        Err(e) => {
            res.render(Json(CheckResponse {
                success: false,
                suite: Some(body.suite),
                exit_code: None,
                duration_ms: None,
                stdout_tail: None,
                stderr_tail: None,
                truncated: false,
                error: Some(e),
            }));
            return;
        }
    };
    let (code, stdout, stderr, duration_ms) =
        run_project_cmd(proj, &cmd, CHECK_TIMEOUT_SECS, projects.ssh.as_ref());
    let (stdout_tail, stdout_trunc) = sanitize_tail(&stdout, MAX_OUTPUT_LEN);
    let (stderr_tail, stderr_trunc) = sanitize_tail(&stderr, MAX_OUTPUT_LEN);
    let truncated = stdout_trunc || stderr_trunc;
    tracing::info!(
        target: "codex.metrics",
        operation = "runProjectCheck",
        project = %body.project,
        suite = %body.suite,
        executor = if proj.is_ssh() { "ssh" } else { "local" },
        success = code == 0,
        exit_code = code,
        duration_ms = duration_ms,
        ssh_calls = if proj.is_ssh() { 1 } else { 0 },
        control_master = projects.ssh.as_ref().map(|s| s.control_master).unwrap_or(false),
        "codex_check_completed"
    );

    res.render(Json(CheckResponse {
        success: code == 0,
        suite: Some(body.suite),
        exit_code: Some(code),
        duration_ms: Some(duration_ms),
        stdout_tail: Some(stdout_tail),
        stderr_tail: Some(stderr_tail),
        truncated,
        error: None,
    }));
}
