use crate::action_audit_sessions::{
    record_action_event, request_action_session_id, ActionAuditEventInput,
    ActionAuditWorkflowLinkInput, WorkflowSessionRelation,
};
use crate::auth::AuthContext;
use crate::get_db;
use salvo::prelude::*;
use serde_json::{json, Value};
use std::time::Instant;

pub struct ActionAudit {
    db: Option<std::sync::Arc<crate::Database>>,
    explicit_session_id: Option<String>,
    principal_kind: Option<String>,
    principal_user_id: Option<String>,
    oauth_client_id: Option<String>,
    endpoint: &'static str,
    action_name: &'static str,
    started_at: i64,
    started_at_ms: i64,
    started_instant: Instant,
    client_window_key: Option<String>,
    client_window_source: Option<String>,
    server_trace_id: Option<String>,
    principal_correlation_kind: Option<String>,
    principal_correlation_id: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ActionAuditRecordTiming {
    ended_at: i64,
    ended_at_ms: i64,
    duration_ms: i64,
}

impl ActionAudit {
    pub fn start(
        req: &Request,
        depot: &Depot,
        endpoint: &'static str,
        action_name: &'static str,
    ) -> Self {
        let auth = depot.obtain::<AuthContext>().ok();
        let (principal_kind, principal_user_id, oauth_client_id) =
            action_principal_attribution(auth);
        let (principal_correlation_kind, principal_correlation_id) =
            crate::tool_runtime::runtime_observation_principal(auth)
                .map(|(kind, id)| (Some(kind), Some(id)))
                .unwrap_or((None, None));
        let now = chrono::Utc::now();
        Self {
            db: get_db(depot),
            explicit_session_id: request_action_session_id(req),
            principal_kind,
            principal_user_id,
            oauth_client_id,
            endpoint,
            action_name,
            started_at: now.timestamp(),
            started_at_ms: now.timestamp_millis(),
            started_instant: Instant::now(),
            client_window_key: None,
            client_window_source: None,
            server_trace_id: None,
            principal_correlation_kind,
            principal_correlation_id,
        }
    }

    /// Attach only the canonical adapter-resolved Window identity and safe trace
    /// correlation. Raw host `_meta["openai/session"]` never reaches this type.
    pub fn with_window(
        mut self,
        window: Option<&crate::client_window::ClientWindow>,
        server_trace_id: Option<&str>,
    ) -> Self {
        if let Some(window) = window {
            self.client_window_key = Some(window.key().to_string());
            self.client_window_source = Some(window.source().to_string());
        }
        self.server_trace_id = server_trace_id.map(str::to_string);
        self
    }

    pub fn record(&self, event: ActionAuditRecord) {
        self.record_inner(event, self.capture_record_timing(), None);
    }

    pub(crate) fn capture_record_timing(&self) -> ActionAuditRecordTiming {
        let ended = chrono::Utc::now();
        ActionAuditRecordTiming {
            ended_at: ended.timestamp(),
            ended_at_ms: ended.timestamp_millis(),
            duration_ms: self
                .started_instant
                .elapsed()
                .as_millis()
                .min(i64::MAX as u128) as i64,
        }
    }

    pub(crate) fn record_with_completion(
        &self,
        event: ActionAuditRecord,
        audit_timing: ActionAuditRecordTiming,
        timing: crate::tool_request_trace::RequestCompletionTiming,
        transition: Option<crate::tool_runtime::WindowLoopTransition>,
        streaming: bool,
        continuity_eligible: bool,
    ) {
        self.record_inner(
            event,
            audit_timing,
            Some((timing, transition, streaming, continuity_eligible)),
        );
    }

    fn record_inner(
        &self,
        event: ActionAuditRecord,
        audit_timing: ActionAuditRecordTiming,
        completion: Option<(
            crate::tool_request_trace::RequestCompletionTiming,
            Option<crate::tool_runtime::WindowLoopTransition>,
            bool,
            bool,
        )>,
    ) {
        let Some(db) = self.db.as_ref() else {
            return;
        };
        record_action_event(
            db,
            ActionAuditEventInput {
                explicit_session_id: self.explicit_session_id.clone(),
                session_title: None,
                endpoint: self.endpoint.to_string(),
                action_name: self.action_name.to_string(),
                operation: event.operation,
                project: event.project,
                principal_kind: self.principal_kind.clone(),
                principal_user_id: self.principal_user_id.clone(),
                oauth_client_id: self.oauth_client_id.clone(),
                status: event.status,
                http_status: Some(event.http_status.as_u16() as i64),
                started_at: self.started_at,
                ended_at: audit_timing.ended_at,
                duration_ms: audit_timing.duration_ms,
                error_summary: event.error_summary,
                warning_summary: event.warning_summary,
                changed_files: event.changed_files,
                ids: event.ids,
                summary: event.summary,
                request_bytes: None,
                response_bytes: None,
                client_window_key: self.client_window_key.clone(),
                client_window_source: self.client_window_source.clone(),
                server_trace_id: self.server_trace_id.clone(),
                principal_correlation_kind: self.principal_correlation_kind.clone(),
                principal_correlation_id: self.principal_correlation_id.clone(),
                window_started_at_ms: self.client_window_key.as_ref().map(|_| self.started_at_ms),
                window_ended_at_ms: self
                    .client_window_key
                    .as_ref()
                    .map(|_| audit_timing.ended_at_ms),
                request_observed_at_ms: completion
                    .as_ref()
                    .map(|(timing, _, _, _)| timing.request_observed_at_ms),
                response_handed_at_ms: completion
                    .as_ref()
                    .map(|(timing, _, _, _)| timing.response_handed_at_ms),
                window_transition_kind: completion.as_ref().and_then(|(_, transition, _, _)| {
                    transition.map(|transition| transition.as_str().to_string())
                }),
                response_streaming: completion.as_ref().map(|(_, _, streaming, _)| *streaming),
                window_continuity_eligible: completion
                    .as_ref()
                    .map(|(_, _, _, eligible)| *eligible && event.window_meaningful),
                window_meaningful: event.window_meaningful,
                recorder_gap_session_id: event.recorder_gap_session_id,
                workflow_links: event.workflow_links,
            },
        );
    }
}

fn action_principal_attribution(
    auth: Option<&AuthContext>,
) -> (Option<String>, Option<String>, Option<String>) {
    let Some(auth) = auth else {
        return (None, None, None);
    };
    let oauth_client_id = auth
        .is_oauth_token()
        .then(|| auth.allowed_client_id.clone())
        .flatten();
    (
        Some(auth.principal_kind().to_string()),
        auth.user_id.clone(),
        oauth_client_id,
    )
}

pub struct ActionAuditRecord {
    pub operation: Option<String>,
    pub project: Option<String>,
    pub status: String,
    pub http_status: StatusCode,
    pub error_summary: Option<String>,
    pub warning_summary: Option<String>,
    pub changed_files: Vec<String>,
    pub ids: Value,
    pub summary: Value,
    pub window_meaningful: bool,
    pub recorder_gap_session_id: Option<String>,
    pub workflow_links: Vec<ActionAuditWorkflowLinkInput>,
}

impl ActionAuditRecord {
    pub fn new(operation: impl Into<String>, success: bool, http_status: StatusCode) -> Self {
        Self {
            operation: Some(operation.into()),
            project: None,
            status: action_status(success, http_status),
            http_status,
            error_summary: None,
            warning_summary: None,
            changed_files: Vec::new(),
            ids: json!({}),
            summary: json!({}),
            window_meaningful: false,
            recorder_gap_session_id: None,
            workflow_links: Vec::new(),
        }
    }

    pub fn meaningful(mut self, meaningful: bool) -> Self {
        self.window_meaningful = meaningful;
        self
    }

    pub fn recorder_gap(mut self, session_id: Option<String>) -> Self {
        self.recorder_gap_session_id = session_id;
        self
    }

    pub fn workflow_link(
        mut self,
        workflow_session_id: impl Into<String>,
        relation: WorkflowSessionRelation,
        project: Option<String>,
    ) -> Self {
        self.workflow_links.push(ActionAuditWorkflowLinkInput {
            workflow_session_id: workflow_session_id.into(),
            relation,
            project,
        });
        self
    }

    pub fn error(mut self, error: Option<String>) -> Self {
        self.error_summary = error;
        self
    }

    pub fn ids(mut self, ids: Value) -> Self {
        self.ids = ids;
        self
    }

    pub fn summary(mut self, summary: Value) -> Self {
        self.summary = summary;
        self
    }
}

pub fn action_status(success: bool, http_status: StatusCode) -> String {
    if success {
        return "success".to_string();
    }
    if http_status == StatusCode::REQUEST_TIMEOUT {
        "timeout".to_string()
    } else {
        "failed".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthContext, AuthKind};

    #[test]
    fn audit_record_timing_preserves_subsecond_monotonic_precision() {
        let now = chrono::Utc::now();
        let audit = ActionAudit {
            db: None,
            explicit_session_id: None,
            principal_kind: None,
            principal_user_id: None,
            oauth_client_id: None,
            endpoint: "/mcp",
            action_name: "toolsCall",
            started_at: now.timestamp(),
            started_at_ms: now.timestamp_millis(),
            started_instant: Instant::now(),
            client_window_key: None,
            client_window_source: None,
            server_trace_id: None,
            principal_correlation_kind: None,
            principal_correlation_id: None,
        };
        std::thread::sleep(std::time::Duration::from_millis(12));
        let timing = audit.capture_record_timing();
        assert!(
            timing.duration_ms > 0,
            "sub-second audit work must not quantize to zero"
        );
        assert!(
            timing.duration_ms < 1_000,
            "test audit unexpectedly exceeded one second"
        );
    }

    #[test]
    fn agent_allowed_client_id_is_not_oauth_client_attribution() {
        let mut auth = AuthContext::new(AuthKind::AgentToken);
        auth.user_id = Some("user-1".to_string());
        auth.allowed_client_id = Some("runner-1".to_string());

        let (principal_kind, principal_user_id, oauth_client_id) =
            action_principal_attribution(Some(&auth));

        assert_eq!(principal_kind.as_deref(), Some("agent_token"));
        assert_eq!(principal_user_id.as_deref(), Some("user-1"));
        assert_eq!(oauth_client_id, None);
    }
}
