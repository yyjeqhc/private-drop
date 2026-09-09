use crate::models::ActionEventWorkflowLinkRecord;
use crate::{ActionEventRecord, ActionSessionRecord, Database};
use salvo::prelude::*;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub const ACTION_SESSION_IDLE_TIMEOUT_SECS: i64 = 1800;
const MAX_SUMMARY_TEXT: usize = 500;
const MAX_PREVIEW_TEXT: usize = 120;

#[derive(Debug, Clone, Serialize, Default)]
pub struct ActionAttributionStats {
    pub by_principal_kind: BTreeMap<String, i64>,
    pub attributed_count: i64,
    pub unattributed_count: i64,
    pub by_oauth_client: Vec<OAuthClientActionStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OAuthClientActionStats {
    pub client_id: String,
    pub name: Option<String>,
    pub count: i64,
}

/// Aggregate audit statistics over a set of decoded action events. Returned by
/// the read-only `POST /api/audit/stats` endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct ActionSessionStats {
    pub by_endpoint: BTreeMap<String, i64>,
    pub by_project: BTreeMap<String, i64>,
    pub by_status: BTreeMap<String, i64>,
    pub attribution: ActionAttributionStats,
    pub edit_count: i64,
    pub context_count: i64,
    pub job_count: i64,
    pub command_count: i64,
    pub report_count: i64,
    pub artifact_count: i64,
    pub git_count: i64,
    pub shell_count: i64,
    pub changed_files_distinct_count: usize,
    pub job_ids_distinct_count: usize,
    /// Number of events actually included in the aggregate.
    pub events_scanned: usize,
    /// Number of events present in the selected session scope before per-session caps.
    pub events_available: usize,
    /// Number of selected sessions included in the aggregate.
    pub sessions_scanned: usize,
    /// Selected sessions whose event history exceeded the bounded scan cap.
    pub truncated_sessions: usize,
    /// True when the aggregate does not cover every event in the selected sessions.
    pub partial: bool,
    /// True when bounded event caps caused the partial result.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionEventView {
    pub event_id: String,
    pub session_id: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub duration_ms: i64,
    pub endpoint: String,
    pub operation: Option<String>,
    pub action_name: String,
    pub project: Option<String>,
    pub status: String,
    pub http_status: Option<i64>,
    pub error_summary: Option<String>,
    pub warning_summary: Option<String>,
    pub changed_files: Vec<String>,
    pub ids: Value,
    pub summary: Value,
    pub request_bytes: Option<i64>,
    pub response_bytes: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowSessionRelation {
    Recording,
    WorkOnProject,
}

impl WorkflowSessionRelation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::WorkOnProject => "work_on_project",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionAuditWorkflowLinkInput {
    pub workflow_session_id: String,
    pub relation: WorkflowSessionRelation,
    pub project: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ActionAuditEventInput {
    pub explicit_session_id: Option<String>,
    pub session_title: Option<String>,
    pub endpoint: String,
    pub action_name: String,
    pub operation: Option<String>,
    pub project: Option<String>,
    pub principal_kind: Option<String>,
    pub principal_user_id: Option<String>,
    pub oauth_client_id: Option<String>,
    pub status: String,
    pub http_status: Option<i64>,
    pub started_at: i64,
    pub ended_at: i64,
    pub duration_ms: i64,
    pub error_summary: Option<String>,
    pub warning_summary: Option<String>,
    pub changed_files: Vec<String>,
    pub ids: Value,
    pub summary: Value,
    pub request_bytes: Option<i64>,
    pub response_bytes: Option<i64>,
    pub client_window_key: Option<String>,
    pub client_window_source: Option<String>,
    pub server_trace_id: Option<String>,
    pub principal_correlation_kind: Option<String>,
    pub principal_correlation_id: Option<String>,
    pub window_started_at_ms: Option<i64>,
    pub window_ended_at_ms: Option<i64>,
    /// Canonical adapter timing for ordinary completed responses. These are
    /// distinct from the legacy ActionAudit record window above.
    pub request_observed_at_ms: Option<i64>,
    pub response_handed_at_ms: Option<i64>,
    /// Process-local continuity classification captured at request arrival.
    /// `None` means the event predates this projection or had no eligible
    /// Window/principal continuity identity.
    pub window_transition_kind: Option<String>,
    /// `None` for legacy rows; true streams are excluded from ordinary response
    /// completion/gap semantics.
    pub response_streaming: Option<bool>,
    /// Whether this completed adapter response may become the previous
    /// meaningful call for a later Window transition. Hard timeouts and streams
    /// are deliberately false.
    pub window_continuity_eligible: Option<bool>,
    pub window_meaningful: bool,
    pub recorder_gap_session_id: Option<String>,
    pub workflow_links: Vec<ActionAuditWorkflowLinkInput>,
}

pub fn trim_and_truncate(value: &str, max_len: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= max_len {
        return trimmed.to_string();
    }
    trimmed.chars().take(max_len).collect::<String>()
}

pub fn redact_text(value: &str) -> String {
    let trimmed = trim_and_truncate(value, MAX_SUMMARY_TEXT);
    if secret_like_value(&trimmed) {
        "[redacted]".to_string()
    } else {
        trimmed
    }
}

pub fn sanitize_error_summary(value: Option<&str>) -> Option<String> {
    value
        .map(redact_text)
        .filter(|text| !text.is_empty())
        .map(|text| trim_and_truncate(&text, MAX_SUMMARY_TEXT))
}

pub fn sanitize_value(value: &Value) -> Value {
    sanitize_value_inner(value, false)
}

/// Stricter read-time sanitization used by the audit query API. Unlike
/// [`sanitize_value`], secret-like keys are dropped entirely (not kept with a
/// `[redacted]` placeholder) so an audit response never echoes sensitive field
/// names.
pub fn sanitize_value_for_read(value: &Value) -> Value {
    sanitize_value_inner(value, true)
}

fn sanitize_value_inner(value: &Value, drop_secrets: bool) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, val) in map {
                if secret_like_key(key) {
                    if drop_secrets {
                        continue;
                    }
                    out.insert(key.clone(), Value::String("[redacted]".to_string()));
                    continue;
                }
                if matches!(
                    key.as_str(),
                    "stdout"
                        | "stderr"
                        | "stdout_tail"
                        | "stderr_tail"
                        | "diff"
                        | "openapi_json"
                        | "text"
                        | "base64_content"
                ) {
                    continue;
                }
                if matches!(key.as_str(), "script_text" | "command_text") {
                    if let Some(text) = val.as_str() {
                        out.insert(key.clone(), summarize_command_text(key, text));
                    } else {
                        out.insert(key.clone(), sanitize_value_inner(val, drop_secrets));
                    }
                    continue;
                }
                out.insert(key.clone(), sanitize_value_inner(val, drop_secrets));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| sanitize_value_inner(v, drop_secrets))
                .collect(),
        ),
        Value::String(text) => Value::String(redact_text(text)),
        _ => value.clone(),
    }
}

pub fn secret_like_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    [
        "token",
        "api_key",
        "apikey",
        "authorization",
        "secret",
        "password",
        "ssh_key",
        "private_key",
        "pem",
        "id_rsa",
        "id_ed25519",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub use webcodex_core::sensitive_text::secret_like_value;

pub fn summarize_command_text(kind: &str, text: &str) -> Value {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let first_line = text.lines().next().unwrap_or_default();
    json!({
        "command_kind": kind,
        "line_count": text.lines().count(),
        "char_count": text.chars().count(),
        "sha256": hash,
        "first_line_preview": trim_and_truncate(&redact_text(first_line), MAX_PREVIEW_TEXT),
    })
}

pub fn request_action_session_id(req: &Request) -> Option<String> {
    req.headers()
        .get("x-action-session-id")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            req.headers()
                .get("x-webcodex-session-id")
                .and_then(|v| v.to_str().ok())
        })
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| trim_and_truncate(v, 120))
        .or_else(|| {
            req.query::<String>("action_session_id")
                .map(|v| trim_and_truncate(v.trim(), 120))
                .filter(|v| !v.is_empty())
        })
}

pub fn get_or_create_active_session(
    db: &Database,
    explicit_session_id: Option<&str>,
    title: Option<&str>,
    now: i64,
) -> anyhow::Result<ActionSessionRecord> {
    if let Some(session_id) = explicit_session_id.filter(|id| !id.trim().is_empty()) {
        if let Some(existing) = db.get_action_session(session_id)? {
            return Ok(existing);
        }
        let record = new_session_record(session_id, title, now);
        db.insert_action_session(&record)?;
        return Ok(record);
    }

    if let Some(existing) =
        db.find_recent_open_action_session(now - ACTION_SESSION_IDLE_TIMEOUT_SECS)?
    {
        return Ok(existing);
    }

    let session_id = uuid::Uuid::new_v4().to_string();
    let record = new_session_record(&session_id, title, now);
    db.insert_action_session(&record)?;
    Ok(record)
}

fn new_session_record(session_id: &str, title: Option<&str>, now: i64) -> ActionSessionRecord {
    ActionSessionRecord {
        session_id: session_id.to_string(),
        title: title
            .map(|v| trim_and_truncate(v, 120))
            .filter(|v| !v.is_empty()),
        note: None,
        status: "open".to_string(),
        created_at: now,
        updated_at: now,
        closed_at: None,
        first_event_at: None,
        last_event_at: None,
        total_actions: 0,
        success_count: 0,
        failed_count: 0,
        timeout_or_unknown_count: 0,
        warning_count: 0,
        total_duration_ms: 0,
        changed_files_count: 0,
        job_ids_count: 0,
    }
}

pub fn record_action_event(db: &Arc<Database>, input: ActionAuditEventInput) {
    if let Err(e) = record_action_event_inner(db, input) {
        tracing::warn!("failed to record action event: {}", e);
    }
}

fn record_action_event_inner(db: &Database, input: ActionAuditEventInput) -> anyhow::Result<()> {
    let now = input.ended_at.max(input.started_at);
    let session = get_or_create_active_session(
        db,
        input.explicit_session_id.as_deref(),
        input.session_title.as_deref(),
        now,
    )?;

    let changed_files = input
        .changed_files
        .into_iter()
        .map(|p| trim_and_truncate(&p, 300))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>();
    let ids = sanitize_value(&input.ids);
    let summary = sanitize_value(&input.summary);
    let changed_files_count = changed_files.len() as i64;
    let job_ids_count = count_job_ids(&ids) as i64;
    let warning_inc = input
        .warning_summary
        .as_ref()
        .map(|s| !s.is_empty())
        .unwrap_or(false) as i64;

    let event_id = uuid::Uuid::new_v4().to_string();
    let window_ended_at_ms = input
        .window_ended_at_ms
        .or(input.window_started_at_ms)
        .map(|ended| ended.max(input.window_started_at_ms.unwrap_or(ended)));
    let event = ActionEventRecord {
        event_id: event_id.clone(),
        session_id: session.session_id,
        started_at: input.started_at,
        ended_at: input.ended_at,
        duration_ms: input.duration_ms.max(0),
        endpoint: input.endpoint,
        operation: input.operation.map(|v| trim_and_truncate(&v, 80)),
        action_name: trim_and_truncate(&input.action_name, 120),
        project: input.project.map(|v| trim_and_truncate(&v, 512)),
        principal_kind: input.principal_kind,
        principal_user_id: input.principal_user_id,
        oauth_client_id: input.oauth_client_id,
        status: input.status.clone(),
        http_status: input.http_status,
        error_summary: sanitize_error_summary(input.error_summary.as_deref()),
        warning_summary: sanitize_error_summary(input.warning_summary.as_deref()),
        changed_files_json: serde_json::to_string(&changed_files)?,
        ids_json: serde_json::to_string(&ids)?,
        summary_json: serde_json::to_string(&summary)?,
        request_bytes: input.request_bytes,
        response_bytes: input.response_bytes,
        client_window_key: input
            .client_window_key
            .map(|value| trim_and_truncate(&value, 128)),
        client_window_source: input
            .client_window_source
            .map(|value| trim_and_truncate(&value, 64)),
        server_trace_id: input
            .server_trace_id
            .map(|value| trim_and_truncate(&value, 128)),
        principal_correlation_kind: input
            .principal_correlation_kind
            .map(|value| trim_and_truncate(&value, 64)),
        principal_correlation_id: input
            .principal_correlation_id
            .map(|value| trim_and_truncate(&value, 512)),
        window_started_at_ms: input.window_started_at_ms,
        window_ended_at_ms,
        request_observed_at_ms: input.request_observed_at_ms,
        response_handed_at_ms: input.response_handed_at_ms,
        window_transition_kind: input
            .window_transition_kind
            .map(|value| trim_and_truncate(&value, 32)),
        response_streaming: input.response_streaming,
        window_continuity_eligible: input.window_continuity_eligible,
        window_meaningful: input.window_meaningful,
        recorder_gap_session_id: input
            .recorder_gap_session_id
            .map(|value| trim_and_truncate(&value, 128)),
    };
    let workflow_links = input
        .workflow_links
        .into_iter()
        .map(|link| ActionEventWorkflowLinkRecord {
            event_id: event_id.clone(),
            workflow_session_id: trim_and_truncate(&link.workflow_session_id, 128),
            workflow_session_relation: link.relation.as_str().to_string(),
            project: link.project.map(|project| trim_and_truncate(&project, 512)),
            linked_at_ms: window_ended_at_ms.unwrap_or_else(|| event.ended_at.saturating_mul(1000)),
        })
        .collect::<Vec<_>>();

    let (success_inc, failed_inc, timeout_inc) = match event.status.as_str() {
        "success" => (1, 0, 0),
        "timeout" | "unknown" => (0, 0, 1),
        _ => (0, 1, 0),
    };
    db.append_action_event_and_update_session(
        &event,
        &workflow_links,
        success_inc,
        failed_inc,
        timeout_inc,
        warning_inc,
        event.duration_ms,
        changed_files_count,
        job_ids_count,
    )?;
    Ok(())
}

fn count_job_ids(ids: &Value) -> usize {
    let mut out = BTreeSet::new();
    if let Some(job_id) = ids.get("job_id").and_then(Value::as_str) {
        out.insert(job_id.to_string());
    }
    if let Some(items) = ids.get("job_ids").and_then(Value::as_array) {
        for id in items.iter().filter_map(Value::as_str) {
            out.insert(id.to_string());
        }
    }
    out.len()
}

pub fn decode_event(record: ActionEventRecord) -> ActionEventView {
    let changed_files =
        serde_json::from_str::<Vec<String>>(&record.changed_files_json).unwrap_or_default();
    let ids = serde_json::from_str::<Value>(&record.ids_json).unwrap_or_else(|_| json!({}));
    let summary = serde_json::from_str::<Value>(&record.summary_json).unwrap_or_else(|_| json!({}));
    ActionEventView {
        event_id: record.event_id,
        session_id: record.session_id,
        started_at: record.started_at,
        ended_at: record.ended_at,
        duration_ms: record.duration_ms,
        endpoint: record.endpoint,
        operation: record.operation,
        action_name: record.action_name,
        project: record.project,
        status: record.status,
        http_status: record.http_status,
        error_summary: record.error_summary,
        warning_summary: record.warning_summary,
        changed_files,
        ids,
        summary,
        request_bytes: record.request_bytes,
        response_bytes: record.response_bytes,
    }
}

pub fn compute_attribution_stats(records: &[ActionEventRecord]) -> ActionAttributionStats {
    let mut by_principal_kind = BTreeMap::new();
    let mut oauth_client_counts = BTreeMap::new();
    let mut attributed_count = 0i64;
    let mut unattributed_count = 0i64;

    for record in records {
        if let Some(kind) = record
            .principal_kind
            .as_deref()
            .map(str::trim)
            .filter(|kind| !kind.is_empty())
        {
            *by_principal_kind.entry(kind.to_string()).or_insert(0) += 1;
            attributed_count += 1;
        } else {
            unattributed_count += 1;
        }
        if let Some(client_id) = record
            .oauth_client_id
            .as_deref()
            .map(str::trim)
            .filter(|client_id| !client_id.is_empty())
        {
            *oauth_client_counts
                .entry(client_id.to_string())
                .or_insert(0) += 1;
        }
    }

    let mut by_oauth_client = oauth_client_counts
        .into_iter()
        .map(|(client_id, count)| OAuthClientActionStats {
            client_id,
            name: None,
            count,
        })
        .collect::<Vec<_>>();
    by_oauth_client.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.client_id.cmp(&right.client_id))
    });

    ActionAttributionStats {
        by_principal_kind,
        attributed_count,
        unattributed_count,
        by_oauth_client,
    }
}

pub fn compute_stats(events: &[ActionEventView]) -> ActionSessionStats {
    let mut by_endpoint = BTreeMap::new();
    let mut by_project = BTreeMap::new();
    let mut by_status = BTreeMap::new();
    let mut changed_files = BTreeSet::new();
    let mut job_ids = BTreeSet::new();
    let mut session_ids = BTreeSet::new();
    let mut edit_count = 0;
    let mut context_count = 0;
    let mut job_count = 0;
    let mut command_count = 0;
    let mut report_count = 0;
    let mut artifact_count = 0;
    let mut git_count = 0;
    let mut shell_count = 0;
    for event in events {
        session_ids.insert(event.session_id.clone());
        *by_endpoint.entry(event.endpoint.clone()).or_insert(0) += 1;
        *by_status.entry(event.status.clone()).or_insert(0) += 1;
        if let Some(project) = &event.project {
            *by_project.entry(project.clone()).or_insert(0) += 1;
        }
        for path in &event.changed_files {
            changed_files.insert(path.clone());
        }
        if let Some(job_id) = event.ids.get("job_id").and_then(Value::as_str) {
            job_ids.insert(job_id.to_string());
        }
        if let Some(ids_arr) = event.ids.get("job_ids").and_then(Value::as_array) {
            for id in ids_arr.iter().filter_map(Value::as_str) {
                job_ids.insert(id.to_string());
            }
        }
        use crate::route_metadata::AuditClass;
        match crate::route_metadata::audit_class_for_path(&event.endpoint) {
            Some(AuditClass::Edit) => edit_count += 1,
            Some(AuditClass::Context) => context_count += 1,
            Some(AuditClass::Job) => job_count += 1,
            Some(AuditClass::Command) => command_count += 1,
            Some(AuditClass::Report) => report_count += 1,
            Some(AuditClass::Artifact) => artifact_count += 1,
            Some(AuditClass::Git) => git_count += 1,
            Some(AuditClass::Shell) => shell_count += 1,
            Some(AuditClass::Other) => {}
            // Historical records can contain endpoints removed before the
            // canonical route registry existed. Keep only those compatibility
            // classifications here; current mounted routes must use RouteSpec.
            None if event.endpoint == "/api/projects/write_file" => edit_count += 1,
            None if event.endpoint == "/api/shell/clients" => shell_count += 1,
            None => {}
        }
    }
    ActionSessionStats {
        by_endpoint,
        by_project,
        by_status,
        attribution: ActionAttributionStats::default(),
        edit_count,
        context_count,
        job_count,
        command_count,
        report_count,
        artifact_count,
        git_count,
        shell_count,
        changed_files_distinct_count: changed_files.len(),
        job_ids_distinct_count: job_ids.len(),
        events_scanned: events.len(),
        events_available: events.len(),
        sessions_scanned: session_ids.len(),
        truncated_sessions: 0,
        partial: false,
        truncated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use webcodex_core::sensitive_text::WEBCODEX_SECRET_PREFIXES;

    #[test]
    fn audit_sanitize_value_redacts_webcodex_token_prefixes_in_strings() {
        for prefix in WEBCODEX_SECRET_PREFIXES {
            let value = format!("failed with token {}EXAMPLE", prefix.to_ascii_uppercase());
            assert_eq!(sanitize_value(&json!(value)), json!("[redacted]"));
        }
    }

    #[test]
    fn audit_sanitize_value_redacts_webcodex_token_prefixes_nested() {
        let value = json!({
            "outer": [
                "ok",
                {"message": "pairing failed for wc_pair_SECRET"},
                ["agent token WC_AGENT_SECRET"]
            ]
        });

        assert_eq!(
            sanitize_value(&value),
            json!({
                "outer": [
                    "ok",
                    {"message": "[redacted]"},
                    ["[redacted]"]
                ]
            })
        );
    }

    #[test]
    fn compute_stats_preserves_removed_endpoint_categories() {
        fn event(endpoint: &str) -> ActionEventView {
            ActionEventView {
                event_id: "evt".to_string(),
                session_id: "session".to_string(),
                started_at: 0,
                ended_at: 0,
                duration_ms: 0,
                endpoint: endpoint.to_string(),
                operation: None,
                action_name: "legacy".to_string(),
                project: None,
                status: "ok".to_string(),
                http_status: None,
                error_summary: None,
                warning_summary: None,
                changed_files: Vec::new(),
                ids: json!({}),
                summary: json!({}),
                request_bytes: None,
                response_bytes: None,
            }
        }

        let stats = compute_stats(&[
            event("/api/projects/write_file"),
            event("/api/shell/clients"),
        ]);
        assert_eq!(stats.edit_count, 1);
        assert_eq!(stats.shell_count, 1);
    }

    #[test]
    fn audit_sanitize_value_keeps_non_secret_regular_strings() {
        assert_eq!(
            sanitize_value(&json!({"message": "normal project status update"})),
            json!({"message": "normal project status update"})
        );
    }
}
