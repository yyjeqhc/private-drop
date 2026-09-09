//! SQLite authority for the project-bound connector task model.
//!
//! These tables deliberately do not mirror or dual-write the legacy workflow
//! session ledger. A connector task is the product-level unit of work; a run is
//! one executor attempt; events are its bounded, ordered audit trail.

use super::execution_model::ConnectorExecutionState;
use super::Database;
use rusqlite::types::Type;
use rusqlite::{params, OptionalExtension, Transaction};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use uuid::Uuid;
use webcodex_core::project_context_contract::ProjectContextFingerprint;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorTaskMode {
    Normal,
    ReadOnly,
    #[serde(rename = "inspect")]
    InspectLegacy,
}

impl ConnectorTaskMode {
    pub const fn as_db(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::ReadOnly => "read_only",
            Self::InspectLegacy => "inspect",
        }
    }

    fn from_db(value: &str, index: usize) -> rusqlite::Result<Self> {
        match value {
            "normal" => Ok(Self::Normal),
            "read_only" => Ok(Self::ReadOnly),
            "inspect" => Ok(Self::InspectLegacy),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                Type::Text,
                format!("unsupported Connector Task mode: {other}").into(),
            )),
        }
    }

    fn requested(value: &str) -> Result<Self, ConnectorTaskStoreError> {
        match value {
            "normal" => Ok(Self::Normal),
            "read_only" => Ok(Self::ReadOnly),
            _ => Err(ConnectorTaskStoreError::InvalidState(
                "task mode must be normal or read_only".to_string(),
            )),
        }
    }
}

impl std::fmt::Display for ConnectorTaskMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_db())
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorTaskLifecycle {
    Active,
    ReadyForReview,
    Accepted,
    Rejected,
}

impl ConnectorTaskLifecycle {
    pub const fn as_db(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::ReadyForReview => "ready_for_review",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }

    fn from_db(value: &str, index: usize) -> rusqlite::Result<Self> {
        match value {
            "active" => Ok(Self::Active),
            "ready_for_review" => Ok(Self::ReadyForReview),
            "accepted" => Ok(Self::Accepted),
            "rejected" => Ok(Self::Rejected),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                Type::Text,
                format!("unsupported Connector Task lifecycle: {other}").into(),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorTaskState {
    Active,
    ReadyForReview,
    Accepted,
    Rejected,
    Cancelled,
    NeedsAttention,
}

impl ConnectorTaskState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::ReadyForReview => "ready_for_review",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
            Self::NeedsAttention => "needs_attention",
        }
    }

    fn from_projection(value: &str, index: usize) -> rusqlite::Result<Self> {
        match value {
            "active" => Ok(Self::Active),
            "ready_for_review" => Ok(Self::ReadyForReview),
            "accepted" => Ok(Self::Accepted),
            "rejected" => Ok(Self::Rejected),
            "cancelled" => Ok(Self::Cancelled),
            "needs_attention" => Ok(Self::NeedsAttention),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                Type::Text,
                format!("unsupported effective Connector Task state: {other}").into(),
            )),
        }
    }
}

impl std::fmt::Display for ConnectorTaskState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorRunLifecycle {
    Running,
    Completed,
    Interrupted,
}

impl ConnectorRunLifecycle {
    pub const fn as_db(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
        }
    }

    fn from_db(value: &str, index: usize) -> rusqlite::Result<Self> {
        match value {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "interrupted" => Ok(Self::Interrupted),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                Type::Text,
                format!("unsupported Connector Run lifecycle: {other}").into(),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorRunState {
    Running,
    Completed,
    Interrupted,
    Cancelled,
}

impl ConnectorRunState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for ConnectorRunState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorResultDecisionStatus {
    Pending,
    Accepted,
    Rejected,
}

impl ConnectorResultDecisionStatus {
    pub const fn as_db(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }

    fn from_db(value: &str, index: usize) -> rusqlite::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "accepted" => Ok(Self::Accepted),
            "rejected" => Ok(Self::Rejected),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                Type::Text,
                format!("unsupported Connector Result decision status: {other}").into(),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorResultDecision {
    Accepted,
    Rejected,
}

impl ConnectorResultDecision {
    pub const fn as_db(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }

    pub fn requested(value: &str) -> Result<Self, ConnectorTaskStoreError> {
        match value {
            "accepted" => Ok(Self::Accepted),
            "rejected" => Ok(Self::Rejected),
            _ => Err(ConnectorTaskStoreError::InvalidState(
                "result decision must be accepted or rejected".to_string(),
            )),
        }
    }

    fn from_db(value: &str, index: usize) -> rusqlite::Result<Self> {
        match value {
            "accepted" => Ok(Self::Accepted),
            "rejected" => Ok(Self::Rejected),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                Type::Text,
                format!("unsupported Connector Result decision: {other}").into(),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorResultDecisionRecoveryState {
    Pending,
    NeedsAttention,
}

impl ConnectorResultDecisionRecoveryState {
    pub const fn as_db(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::NeedsAttention => "needs_attention",
        }
    }

    fn from_db(value: &str, index: usize) -> rusqlite::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "needs_attention" => Ok(Self::NeedsAttention),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                Type::Text,
                format!("unsupported Connector Result decision recovery state: {other}").into(),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorApprovalState {
    Pending,
    Approved,
    Denied,
    Consumed,
    Expired,
}

impl ConnectorApprovalState {
    pub const fn as_db(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Consumed => "consumed",
            Self::Expired => "expired",
        }
    }

    fn from_db(value: &str, index: usize) -> rusqlite::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "denied" => Ok(Self::Denied),
            "consumed" => Ok(Self::Consumed),
            "expired" => Ok(Self::Expired),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                Type::Text,
                format!("unsupported Connector Approval state: {other}").into(),
            )),
        }
    }
}

impl std::fmt::Display for ConnectorApprovalState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_db())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectorEditOperationState {
    Pending,
    Completed,
    Failed,
}

impl ConnectorEditOperationState {
    fn from_db(value: &str, index: usize) -> rusqlite::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                Type::Text,
                format!("unsupported Connector Edit Operation state: {other}").into(),
            )),
        }
    }
}

fn effective_task_state(
    lifecycle: ConnectorTaskLifecycle,
    run_lifecycle: ConnectorRunLifecycle,
    result_decision: Option<ConnectorResultDecisionStatus>,
    cancelled: bool,
) -> ConnectorTaskState {
    if cancelled {
        return ConnectorTaskState::Cancelled;
    }
    match result_decision {
        Some(ConnectorResultDecisionStatus::Accepted) => return ConnectorTaskState::Accepted,
        Some(ConnectorResultDecisionStatus::Rejected) => return ConnectorTaskState::Rejected,
        Some(ConnectorResultDecisionStatus::Pending) | None => {}
    }
    if run_lifecycle == ConnectorRunLifecycle::Interrupted {
        return ConnectorTaskState::NeedsAttention;
    }
    match lifecycle {
        ConnectorTaskLifecycle::Active => ConnectorTaskState::Active,
        ConnectorTaskLifecycle::ReadyForReview => ConnectorTaskState::ReadyForReview,
        ConnectorTaskLifecycle::Accepted => ConnectorTaskState::Accepted,
        ConnectorTaskLifecycle::Rejected => ConnectorTaskState::Rejected,
    }
}

fn effective_run_state(lifecycle: ConnectorRunLifecycle, cancelled: bool) -> ConnectorRunState {
    if cancelled {
        return ConnectorRunState::Cancelled;
    }
    match lifecycle {
        ConnectorRunLifecycle::Running => ConnectorRunState::Running,
        ConnectorRunLifecycle::Completed => ConnectorRunState::Completed,
        ConnectorRunLifecycle::Interrupted => ConnectorRunState::Interrupted,
    }
}

pub struct ConnectorBinding<'a> {
    pub project_id: &'a str,
    pub project_name: &'a str,
    pub workspace_id: &'a str,
    pub executor_ref: &'a str,
    pub subject_id: &'a str,
    pub profile: &'a str,
    pub now: i64,
}

pub struct NewConnectorTask<'a> {
    pub task_id: &'a str,
    pub run_id: &'a str,
    pub project_id: &'a str,
    pub workspace_id: &'a str,
    pub subject_id: &'a str,
    pub goal: &'a str,
    pub mode: &'a str,
    pub target_executor_ref: &'a str,
    pub execution_executor_ref: &'a str,
    pub target_root: &'a str,
    pub execution_root: &'a str,
    pub baseline_commit: Option<&'a str>,
    pub baseline_tree: Option<&'a str>,
    pub isolated: bool,
    pub now: i64,
}

pub struct ConnectorTaskContinuation<'a> {
    pub task_id: &'a str,
    pub project_id: &'a str,
    pub subject_id: &'a str,
    pub instruction: &'a str,
    pub mode: &'a str,
    pub workspace: Option<ConnectorWorkspaceTransition<'a>>,
    pub now: i64,
}

pub struct ConnectorWorkspaceTransition<'a> {
    pub target_executor_ref: &'a str,
    pub execution_executor_ref: &'a str,
    pub target_root: &'a str,
    pub execution_root: &'a str,
    pub baseline_commit: &'a str,
    pub baseline_tree: &'a str,
}

pub struct ConnectorWindowBinding<'a> {
    pub window_key: &'a str,
    pub window_source: &'a str,
    pub project_root_sha256: &'a str,
    pub target_path: &'a str,
    pub fingerprint: &'a ProjectContextFingerprint,
    pub now: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectorWindowContext {
    pub task_id: String,
    pub target_path: String,
    pub fingerprint: ProjectContextFingerprint,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowProjectActivation {
    pub previous_project: Option<String>,
    pub current_project: String,
    pub switched: bool,
}

pub struct NewConnectorResult<'a> {
    pub result_id: &'a str,
    pub summary: &'a str,
    pub patch_artifact: Option<&'a str>,
    pub patch_sha256: Option<&'a str>,
    pub patch_bytes: usize,
    pub changed_paths: &'a [String],
    pub validation: &'a Value,
    pub warnings: &'a [String],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectorPreservedWorkspace {
    pub task_id: String,
    pub run_id: String,
    pub execution_root: String,
    pub execution_executor_ref: String,
    pub baseline_commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ConnectorTaskSnapshot {
    pub task_id: String,
    pub run_id: String,
    pub project_id: String,
    pub workspace_id: String,
    #[serde(skip_serializing)]
    pub owner_subject_id: String,
    pub goal: String,
    pub mode: ConnectorTaskMode,
    #[serde(skip_serializing)]
    pub task_lifecycle: ConnectorTaskLifecycle,
    pub task_status: ConnectorTaskState,
    #[serde(skip_serializing)]
    pub run_lifecycle: ConnectorRunLifecycle,
    pub run_status: ConnectorRunState,
    pub event_cursor: i64,
    #[serde(skip_serializing)]
    pub target_executor_ref: String,
    #[serde(skip_serializing)]
    pub execution_executor_ref: String,
    #[serde(skip_serializing)]
    pub target_root: String,
    #[serde(skip_serializing)]
    pub execution_root: String,
    #[serde(skip_serializing)]
    pub baseline_commit: Option<String>,
    #[serde(skip_serializing)]
    pub baseline_tree: Option<String>,
    pub isolated: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LocalReviewableTask {
    pub task_id: String,
    pub goal: String,
    pub task_status: ConnectorTaskState,
    pub updated_at: i64,
    pub execution_status: Option<ConnectorExecutionState>,
    pub validation_status: Option<String>,
    pub next_action: String,
    /// Count of `human_guidance` events the model has not yet claimed (above
    /// the task's `guidance_seen_seq` watermark). Lets the work queue flag a
    /// task with pending, unread guidance without an extra per-task review.
    pub unread_guidance: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ConnectorTaskResult {
    pub result_id: String,
    pub task_id: String,
    pub run_id: String,
    pub summary: String,
    #[serde(skip_serializing)]
    pub patch_artifact: Option<String>,
    pub patch_sha256: Option<String>,
    pub patch_bytes: usize,
    pub changed_paths: Vec<String>,
    pub validation: Value,
    pub warnings: Vec<String>,
    pub decision_status: ConnectorResultDecisionStatus,
    pub decided_by: Option<String>,
    pub decided_at: Option<i64>,
    pub cleanup_warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<ConnectorResultDecisionRecovery>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ConnectorResultDecisionRecovery {
    pub state: ConnectorResultDecisionRecoveryState,
    pub decision: ConnectorResultDecision,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub last_attempt_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ConnectorApproval {
    pub approval_id: String,
    pub task_id: String,
    pub run_id: String,
    pub action_kind: String,
    pub action_hash: String,
    pub action_summary: String,
    pub state: ConnectorApprovalState,
    pub requested_at: i64,
    pub expires_at: i64,
    pub decided_by: Option<String>,
    pub decided_at: Option<i64>,
    pub consumed_at: Option<i64>,
    pub decision_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConnectorApprovalGate {
    Pending(ConnectorApproval),
    Denied(ConnectorApproval),
    Expired(ConnectorApproval),
    Consumed(ConnectorApproval),
    Authorized(ConnectorApproval),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConnectorEditOperationGate {
    Started,
    Replay(Value),
    Pending,
    Conflict,
}

/// Paths a task has applied, with the total so a bounded list is never
/// mistaken for the whole set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedPaths {
    pub paths: Vec<String>,
    pub total: usize,
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ConnectorTaskEvent {
    pub event_id: String,
    pub sequence: i64,
    pub kind: String,
    pub payload: Value,
    pub created_at: i64,
}

/// Read-state of a task's `human_guidance` events, as seen by the host review
/// console. `seen_seq` is the watermark the model has claimed up to;
/// `last_pending_seq` is the newest guidance still unread, or `None`.
#[derive(Debug, Clone, Copy)]
pub struct GuidanceReadState {
    pub seen_seq: i64,
    pub last_pending_seq: Option<i64>,
}

#[derive(Debug)]
pub enum ConnectorTaskStoreError {
    NotFound,
    OperationIdConflict(String),
    Decision(&'static str, String),
    InvalidState(String),
    Storage(anyhow::Error),
}

impl ConnectorTaskStoreError {
    pub fn decision(code: &'static str, message: impl Into<String>) -> Self {
        Self::Decision(code, message.into())
    }
}

impl From<String> for ConnectorTaskStoreError {
    fn from(message: String) -> Self {
        Self::decision("result_precondition_failed", message)
    }
}

impl std::fmt::Display for ConnectorTaskStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "task not found"),
            Self::OperationIdConflict(operation_id) => {
                write!(
                    f,
                    "operation_id '{operation_id}' was reused with a different request"
                )
            }
            Self::Decision(_, message) => write!(f, "{message}"),
            Self::InvalidState(message) => write!(f, "{message}"),
            Self::Storage(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ConnectorTaskStoreError {}

impl From<rusqlite::Error> for ConnectorTaskStoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Storage(value.into())
    }
}

impl From<serde_json::Error> for ConnectorTaskStoreError {
    fn from(value: serde_json::Error) -> Self {
        Self::Storage(value.into())
    }
}

impl Database {
    /// Process-local navigation state. The durable window/project context map
    /// below is deliberately separate, so a restart cannot invent a current
    /// project but can still recover exact prior work when the next request
    /// names that project.
    pub fn activate_window_project(
        &self,
        subject_id: &str,
        window_key: &str,
        project_identity: &str,
    ) -> WindowProjectActivation {
        let key = (subject_id.to_string(), window_key.to_string());
        let mut bindings = self.window_projects.lock().unwrap();
        let previous_project = bindings.insert(key, project_identity.to_string());
        let switched = previous_project
            .as_deref()
            .is_some_and(|previous| previous != project_identity);
        WindowProjectActivation {
            previous_project,
            current_project: project_identity.to_string(),
            switched,
        }
    }

    pub fn connector_window_context(
        &self,
        window_key: &str,
        project_id: &str,
        subject_id: &str,
        project_root_sha256: &str,
    ) -> Result<Option<ConnectorWindowContext>, ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        let stored = conn
            .query_row(
                "SELECT task_id, target_path, fingerprint_json, created_at, updated_at
                 FROM wc_window_project_contexts
                 WHERE window_key = ?1 AND project_id = ?2 AND owner_subject_id = ?3
                   AND project_root_sha256 = ?4",
                params![window_key, project_id, subject_id, project_root_sha256],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((task_id, target_path, fingerprint_json, created_at, updated_at)) = stored else {
            return Ok(None);
        };
        let fingerprint = serde_json::from_str(&fingerprint_json)?;
        Ok(Some(ConnectorWindowContext {
            task_id,
            target_path,
            fingerprint,
            created_at,
            updated_at,
        }))
    }

    pub fn connector_window_context_for_task(
        &self,
        task_id: &str,
        project_id: &str,
        subject_id: &str,
    ) -> Result<Option<ConnectorWindowContext>, ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        let stored = conn
            .query_row(
                "SELECT task_id, target_path, fingerprint_json, created_at, updated_at
                 FROM wc_window_project_contexts
                 WHERE task_id = ?1 AND project_id = ?2 AND owner_subject_id = ?3",
                params![task_id, project_id, subject_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((task_id, target_path, fingerprint_json, created_at, updated_at)) = stored else {
            return Ok(None);
        };
        let fingerprint = serde_json::from_str(&fingerprint_json)?;
        Ok(Some(ConnectorWindowContext {
            task_id,
            target_path,
            fingerprint,
            created_at,
            updated_at,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn bind_connector_window_context(
        &self,
        window_key: &str,
        window_source: &str,
        project_id: &str,
        subject_id: &str,
        project_root_sha256: &str,
        task_id: &str,
        target_path: &str,
        fingerprint: &ProjectContextFingerprint,
        now: i64,
    ) -> Result<(), ConnectorTaskStoreError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        bind_window_context(
            &tx,
            task_id,
            project_id,
            subject_id,
            ConnectorWindowBinding {
                window_key,
                window_source,
                project_root_sha256,
                target_path,
                fingerprint,
                now,
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn ensure_connector_binding(
        &self,
        binding: ConnectorBinding<'_>,
    ) -> Result<(), ConnectorTaskStoreError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO wc_projects (id, name, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, updated_at = excluded.updated_at",
            params![binding.project_id, binding.project_name, binding.now],
        )?;
        tx.execute(
            "INSERT INTO wc_workspaces (id, project_id, executor_ref, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(id) DO UPDATE SET
                 project_id = excluded.project_id,
                 executor_ref = excluded.executor_ref,
                 updated_at = excluded.updated_at",
            params![
                binding.workspace_id,
                binding.project_id,
                binding.executor_ref,
                binding.now
            ],
        )?;
        tx.execute(
            "INSERT INTO wc_connector_grants
                (id, project_id, subject_id, profile, created_at, updated_at, revoked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, NULL)
             ON CONFLICT(project_id, subject_id) DO UPDATE SET
                 profile = excluded.profile,
                 updated_at = excluded.updated_at,
                 revoked_at = NULL",
            params![
                new_id("wc_cgr"),
                binding.project_id,
                binding.subject_id,
                binding.profile,
                binding.now
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn start_connector_task(
        &self,
        task: NewConnectorTask<'_>,
    ) -> Result<ConnectorTaskSnapshot, ConnectorTaskStoreError> {
        self.start_connector_task_transaction(task, None)
    }

    pub fn start_connector_task_and_bind(
        &self,
        task: NewConnectorTask<'_>,
        binding: ConnectorWindowBinding<'_>,
    ) -> Result<ConnectorTaskSnapshot, ConnectorTaskStoreError> {
        self.start_connector_task_transaction(task, Some(binding))
    }

    fn start_connector_task_transaction(
        &self,
        task: NewConnectorTask<'_>,
        binding: Option<ConnectorWindowBinding<'_>>,
    ) -> Result<ConnectorTaskSnapshot, ConnectorTaskStoreError> {
        let mode = ConnectorTaskMode::requested(task.mode)?;
        validate_connector_task_workspace_shape(
            mode,
            task.isolated,
            task.target_root,
            task.execution_root,
            task.baseline_commit,
            task.baseline_tree,
        )?;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let granted = tx
            .query_row(
                "SELECT 1 FROM wc_connector_grants
                 WHERE project_id = ?1 AND subject_id = ?2 AND revoked_at IS NULL",
                params![task.project_id, task.subject_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !granted {
            return Err(ConnectorTaskStoreError::NotFound);
        }

        tx.execute(
            "INSERT INTO wc_tasks
                (id, project_id, owner_subject_id, goal, mode, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, ?6)",
            params![
                task.task_id,
                task.project_id,
                task.subject_id,
                task.goal,
                mode.as_db(),
                task.now
            ],
        )?;
        tx.execute(
            "INSERT INTO wc_runs (id, task_id, workspace_id, status, started_at, finished_at)
             VALUES (?1, ?2, ?3, 'running', ?4, NULL)",
            params![task.run_id, task.task_id, task.workspace_id, task.now],
        )?;
        tx.execute(
            "INSERT INTO wc_run_contexts
                (run_id, target_executor_ref, execution_executor_ref, target_root,
                 execution_root, baseline_commit, baseline_tree, isolated, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                task.run_id,
                task.target_executor_ref,
                task.execution_executor_ref,
                task.target_root,
                task.execution_root,
                task.baseline_commit,
                task.baseline_tree,
                i64::from(task.isolated),
                task.now
            ],
        )?;
        insert_event(
            &tx,
            task.task_id,
            task.run_id,
            1,
            "task_started",
            &serde_json::json!({
                "goal": task.goal,
                "mode": mode,
                "isolated": task.isolated,
                "baseline_commit": task.baseline_commit
            }),
            task.now,
        )?;
        if let Some(binding) = binding {
            bind_window_context(&tx, task.task_id, task.project_id, task.subject_id, binding)?;
        }
        tx.commit()?;

        Ok(ConnectorTaskSnapshot {
            task_id: task.task_id.to_string(),
            run_id: task.run_id.to_string(),
            project_id: task.project_id.to_string(),
            workspace_id: task.workspace_id.to_string(),
            owner_subject_id: task.subject_id.to_string(),
            goal: task.goal.to_string(),
            mode,
            task_lifecycle: ConnectorTaskLifecycle::Active,
            task_status: ConnectorTaskState::Active,
            run_lifecycle: ConnectorRunLifecycle::Running,
            run_status: ConnectorRunState::Running,
            event_cursor: 1,
            target_executor_ref: task.target_executor_ref.to_string(),
            execution_executor_ref: task.execution_executor_ref.to_string(),
            target_root: task.target_root.to_string(),
            execution_root: task.execution_root.to_string(),
            baseline_commit: task.baseline_commit.map(str::to_string),
            baseline_tree: task.baseline_tree.map(str::to_string),
            isolated: task.isolated,
            created_at: task.now,
            updated_at: task.now,
        })
    }

    pub fn continue_connector_task_and_bind(
        &self,
        continuation: ConnectorTaskContinuation<'_>,
        binding: ConnectorWindowBinding<'_>,
    ) -> Result<(ConnectorTaskSnapshot, i64, ConnectorTaskMode), ConnectorTaskStoreError> {
        self.continue_connector_task_transaction(continuation, Some(binding))
    }

    fn continue_connector_task_transaction(
        &self,
        continuation: ConnectorTaskContinuation<'_>,
        binding: Option<ConnectorWindowBinding<'_>>,
    ) -> Result<(ConnectorTaskSnapshot, i64, ConnectorTaskMode), ConnectorTaskStoreError> {
        let requested_mode = ConnectorTaskMode::requested(continuation.mode)?;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let task = load_task(
            &tx,
            continuation.task_id,
            continuation.project_id,
            continuation.subject_id,
        )?
        .ok_or(ConnectorTaskStoreError::NotFound)?;
        require_running(&task)?;
        if task.mode == ConnectorTaskMode::InspectLegacy {
            return Err(ConnectorTaskStoreError::InvalidState(
                "inspect_mode_retired: this pre-0.4 inspect task can no longer execute; start a new read_only task for analysis or a new normal task for writable work"
                    .to_string(),
            ));
        }
        if task.task_status != ConnectorTaskState::Active {
            return Err(ConnectorTaskStoreError::InvalidState(
                "only an active task can continue".to_string(),
            ));
        }
        if task.mode == ConnectorTaskMode::Normal && requested_mode == ConnectorTaskMode::ReadOnly {
            return Err(ConnectorTaskStoreError::InvalidState(
                "normal tasks cannot transition to read_only; finish or reject the current writable task and start a new read_only task"
                    .to_string(),
            ));
        }
        if requested_mode == ConnectorTaskMode::Normal
            && !task.isolated
            && continuation.workspace.is_none()
        {
            return Err(ConnectorTaskStoreError::InvalidState(
                "read-only workspace must be upgraded before enabling writes".to_string(),
            ));
        }
        let previous_mode = task.mode;
        let workspace_upgraded = continuation.workspace.is_some();
        let (
            final_isolated,
            final_target_root,
            final_execution_root,
            final_baseline_commit,
            final_baseline_tree,
        ) = match continuation.workspace.as_ref() {
            Some(workspace) => (
                true,
                workspace.target_root,
                workspace.execution_root,
                Some(workspace.baseline_commit),
                Some(workspace.baseline_tree),
            ),
            None => (
                task.isolated,
                task.target_root.as_str(),
                task.execution_root.as_str(),
                task.baseline_commit.as_deref(),
                task.baseline_tree.as_deref(),
            ),
        };
        validate_connector_task_workspace_shape(
            requested_mode,
            final_isolated,
            final_target_root,
            final_execution_root,
            final_baseline_commit,
            final_baseline_tree,
        )?;
        if let Some(workspace) = continuation.workspace {
            tx.execute(
                "UPDATE wc_run_contexts
                 SET target_executor_ref = ?1, execution_executor_ref = ?2,
                     target_root = ?3, execution_root = ?4,
                     baseline_commit = ?5, baseline_tree = ?6, isolated = 1
                 WHERE run_id = ?7",
                params![
                    workspace.target_executor_ref,
                    workspace.execution_executor_ref,
                    workspace.target_root,
                    workspace.execution_root,
                    workspace.baseline_commit,
                    workspace.baseline_tree,
                    task.run_id
                ],
            )?;
        }
        tx.execute(
            "UPDATE wc_tasks SET mode = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                requested_mode.as_db(),
                continuation.now,
                continuation.task_id
            ],
        )?;
        let sequence = task.event_cursor + 1;
        insert_event(
            &tx,
            &task.task_id,
            &task.run_id,
            sequence,
            "task_instruction",
            &serde_json::json!({
                "instruction": continuation.instruction,
                "previous_mode": previous_mode,
                "mode": requested_mode,
                "capability_changed": previous_mode != requested_mode,
                "workspace_upgraded": workspace_upgraded
            }),
            continuation.now,
        )?;
        if let Some(binding) = binding {
            bind_window_context(
                &tx,
                &task.task_id,
                continuation.project_id,
                continuation.subject_id,
                binding,
            )?;
        }
        tx.commit()?;
        let task = load_task(
            &conn,
            continuation.task_id,
            continuation.project_id,
            continuation.subject_id,
        )?
        .ok_or(ConnectorTaskStoreError::NotFound)?;
        Ok((task, sequence, previous_mode))
    }

    /// Record a follow-up instruction without pretending an interrupted run
    /// resumed. This is metadata-only and intentionally does not change mode,
    /// run status, or workspace ownership.
    #[allow(clippy::too_many_arguments)]
    pub fn append_interrupted_connector_instruction_and_bind(
        &self,
        task_id: &str,
        project_id: &str,
        subject_id: &str,
        instruction: &str,
        requested_mode: &str,
        now: i64,
        binding: ConnectorWindowBinding<'_>,
    ) -> Result<i64, ConnectorTaskStoreError> {
        self.append_interrupted_connector_instruction_transaction(
            task_id,
            project_id,
            subject_id,
            instruction,
            requested_mode,
            now,
            Some(binding),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn append_interrupted_connector_instruction_transaction(
        &self,
        task_id: &str,
        project_id: &str,
        subject_id: &str,
        instruction: &str,
        requested_mode: &str,
        now: i64,
        binding: Option<ConnectorWindowBinding<'_>>,
    ) -> Result<i64, ConnectorTaskStoreError> {
        let requested_mode = ConnectorTaskMode::requested(requested_mode)?;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let task = load_task(&tx, task_id, project_id, subject_id)?
            .ok_or(ConnectorTaskStoreError::NotFound)?;
        if task.mode == ConnectorTaskMode::InspectLegacy {
            return Err(ConnectorTaskStoreError::InvalidState(
                "inspect_mode_retired: this pre-0.4 inspect task can no longer execute; start a new read_only task for analysis or a new normal task for writable work"
                    .to_string(),
            ));
        }
        if task.mode == ConnectorTaskMode::Normal && requested_mode == ConnectorTaskMode::ReadOnly {
            return Err(ConnectorTaskStoreError::InvalidState(
                "normal tasks cannot transition to read_only; finish or reject the current writable task and start a new read_only task"
                    .to_string(),
            ));
        }
        validate_connector_task_workspace_shape(
            task.mode,
            task.isolated,
            &task.target_root,
            &task.execution_root,
            task.baseline_commit.as_deref(),
            task.baseline_tree.as_deref(),
        )?;
        if task.run_status != ConnectorRunState::Interrupted
            || task.task_status != ConnectorTaskState::NeedsAttention
        {
            return Err(ConnectorTaskStoreError::InvalidState(
                "only an interrupted task accepts a blocked continuation instruction".to_string(),
            ));
        }
        let sequence = task.event_cursor + 1;
        insert_event(
            &tx,
            task_id,
            &task.run_id,
            sequence,
            "task_instruction",
            &serde_json::json!({
                "instruction": instruction,
                "mode": requested_mode,
                "applied": false,
                "blocked_by": "task_interrupted"
            }),
            now,
        )?;
        touch_task(&tx, task_id, now)?;
        if let Some(binding) = binding {
            bind_window_context(&tx, task_id, project_id, subject_id, binding)?;
        }
        tx.commit()?;
        Ok(sequence)
    }

    pub fn connector_task(
        &self,
        task_id: &str,
        project_id: &str,
        subject_id: &str,
    ) -> Result<ConnectorTaskSnapshot, ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        load_task(&conn, task_id, project_id, subject_id)?.ok_or(ConnectorTaskStoreError::NotFound)
    }

    pub fn append_connector_task_event(
        &self,
        task_id: &str,
        project_id: &str,
        subject_id: &str,
        kind: &str,
        payload: &Value,
        now: i64,
    ) -> Result<i64, ConnectorTaskStoreError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let task = load_task(&tx, task_id, project_id, subject_id)?
            .ok_or(ConnectorTaskStoreError::NotFound)?;
        require_running(&task)?;
        let sequence = task.event_cursor + 1;
        insert_event(
            &tx,
            &task.task_id,
            &task.run_id,
            sequence,
            kind,
            payload,
            now,
        )?;
        touch_task(&tx, task_id, now)?;
        tx.commit()?;
        Ok(sequence)
    }

    /// Append a `human_guidance` event on a task that has already left the
    /// running state. Reserved for decision feedback (a rejection reason):
    /// the only reader, `claim_pending_connector_guidance`, never required a
    /// running task, so a terminal-state write stays consistent with it.
    /// Everything else goes through [`Self::append_connector_task_event`],
    /// which enforces the running guard.
    pub fn append_connector_decision_guidance(
        &self,
        task_id: &str,
        project_id: &str,
        subject_id: &str,
        payload: &Value,
        now: i64,
    ) -> Result<i64, ConnectorTaskStoreError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let task = load_task(&tx, task_id, project_id, subject_id)?
            .ok_or(ConnectorTaskStoreError::NotFound)?;
        let sequence = task.event_cursor + 1;
        insert_event(
            &tx,
            &task.task_id,
            &task.run_id,
            sequence,
            "human_guidance",
            payload,
            now,
        )?;
        touch_task(&tx, task_id, now)?;
        tx.commit()?;
        Ok(sequence)
    }

    pub fn begin_connector_edit_operation(
        &self,
        task_id: &str,
        project_id: &str,
        subject_id: &str,
        operation_id: &str,
        request_sha256: &str,
        now: i64,
    ) -> Result<ConnectorEditOperationGate, ConnectorTaskStoreError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let task = load_task(&tx, task_id, project_id, subject_id)?
            .ok_or(ConnectorTaskStoreError::NotFound)?;
        require_running(&task)?;
        let existing = tx
            .query_row(
                "SELECT request_sha256, state, result_json
                 FROM wc_edit_operations
                 WHERE task_id = ?1 AND operation_id = ?2",
                params![task_id, operation_id],
                |row| {
                    let state = row.get::<_, String>(1)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        ConnectorEditOperationState::from_db(&state, 1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((stored_hash, state, result_json)) = existing {
            tx.commit()?;
            if stored_hash != request_sha256 {
                return Ok(ConnectorEditOperationGate::Conflict);
            }
            return match (state, result_json) {
                (ConnectorEditOperationState::Pending, None) => {
                    Ok(ConnectorEditOperationGate::Pending)
                }
                (ConnectorEditOperationState::Completed, Some(result_json)) => Ok(
                    ConnectorEditOperationGate::Replay(serde_json::from_str(&result_json)?),
                ),
                (ConnectorEditOperationState::Failed, None) => {
                    let updated = conn.execute(
                        "UPDATE wc_edit_operations SET state = 'pending', updated_at = ?1
                         WHERE task_id = ?2 AND operation_id = ?3 AND request_sha256 = ?4
                           AND state = 'failed'",
                        params![now, task_id, operation_id, request_sha256],
                    )?;
                    if updated == 1 {
                        Ok(ConnectorEditOperationGate::Started)
                    } else {
                        Ok(ConnectorEditOperationGate::Pending)
                    }
                }
                _ => Err(ConnectorTaskStoreError::InvalidState(
                    "edit operation state is inconsistent".to_string(),
                )),
            };
        }
        tx.execute(
            "INSERT INTO wc_edit_operations
                (task_id, operation_id, request_sha256, state, result_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'pending', NULL, ?4, ?4)",
            params![task_id, operation_id, request_sha256, now],
        )?;
        tx.commit()?;
        Ok(ConnectorEditOperationGate::Started)
    }

    pub fn complete_connector_edit_operation(
        &self,
        task_id: &str,
        project_id: &str,
        subject_id: &str,
        operation_id: &str,
        request_sha256: &str,
        result: &Value,
        now: i64,
    ) -> Result<(), ConnectorTaskStoreError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let task = load_task(&tx, task_id, project_id, subject_id)?
            .ok_or(ConnectorTaskStoreError::NotFound)?;
        require_running(&task)?;
        let updated = tx.execute(
            "UPDATE wc_edit_operations
             SET state = 'completed', result_json = ?1, updated_at = ?2
             WHERE task_id = ?3 AND operation_id = ?4 AND request_sha256 = ?5
               AND state = 'pending'",
            params![
                serde_json::to_string(result)?,
                now,
                task_id,
                operation_id,
                request_sha256
            ],
        )?;
        if updated != 1 {
            return Err(ConnectorTaskStoreError::InvalidState(
                "edit operation could not be completed".to_string(),
            ));
        }
        tx.commit()?;
        Ok(())
    }

    pub fn fail_connector_edit_operation(
        &self,
        task_id: &str,
        operation_id: &str,
        request_sha256: &str,
        now: i64,
    ) -> Result<(), ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        let updated = conn.execute(
            "UPDATE wc_edit_operations SET state = 'failed', updated_at = ?1
             WHERE task_id = ?2 AND operation_id = ?3 AND request_sha256 = ?4
               AND state = 'pending'",
            params![now, task_id, operation_id, request_sha256],
        )?;
        if updated != 1 {
            return Err(ConnectorTaskStoreError::InvalidState(
                "edit operation could not be marked failed".to_string(),
            ));
        }
        Ok(())
    }

    pub fn finish_connector_task(
        &self,
        task_id: &str,
        project_id: &str,
        subject_id: &str,
        result: NewConnectorResult<'_>,
        now: i64,
    ) -> Result<i64, ConnectorTaskStoreError> {
        let patch_metadata_valid = match result.patch_bytes {
            0 => result.patch_artifact.is_none() && result.patch_sha256.is_none(),
            _ => result.patch_artifact.is_some() && result.patch_sha256.is_some(),
        };
        if !patch_metadata_valid {
            return Err(ConnectorTaskStoreError::InvalidState(
                "task result patch metadata is inconsistent".to_string(),
            ));
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let task = load_task(&tx, task_id, project_id, subject_id)?
            .ok_or(ConnectorTaskStoreError::NotFound)?;
        require_running(&task)?;
        let sequence = task.event_cursor + 1;
        insert_event(
            &tx,
            &task.task_id,
            &task.run_id,
            sequence,
            "task_finished",
            &serde_json::json!({
                "result_id": result.result_id,
                "summary": result.summary,
                "patch_sha256": result.patch_sha256,
                "patch_bytes": result.patch_bytes,
                "changed_file_count": result.changed_paths.len(),
                "warning_count": result.warnings.len()
            }),
            now,
        )?;
        tx.execute(
            "INSERT INTO wc_task_results
                (id, task_id, run_id, summary, patch_artifact, patch_sha256, patch_bytes,
                 changed_paths_json, validation_json, warnings_json, decision_status,
                 decided_by, decided_at, cleanup_warning, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending',
                     NULL, NULL, NULL, ?11)",
            params![
                result.result_id,
                task.task_id,
                task.run_id,
                result.summary,
                result.patch_artifact,
                result.patch_sha256,
                result.patch_bytes as i64,
                serde_json::to_string(result.changed_paths)?,
                serde_json::to_string(result.validation)?,
                serde_json::to_string(result.warnings)?,
                now
            ],
        )?;
        tx.execute(
            "UPDATE wc_runs SET status = 'completed', finished_at = ?1 WHERE id = ?2",
            params![now, task.run_id],
        )?;
        tx.execute(
            "UPDATE wc_tasks SET status = 'ready_for_review', updated_at = ?1 WHERE id = ?2",
            params![now, task.task_id],
        )?;
        expire_task_approvals(&tx, &task.task_id)?;
        tx.commit()?;
        Ok(sequence)
    }

    pub fn record_connector_workspace_release(
        &self,
        task_id: &str,
        project_id: &str,
        subject_id: &str,
        released: bool,
        cleanup_warning: Option<&str>,
        now: i64,
    ) -> Result<i64, ConnectorTaskStoreError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let task = load_task(&tx, task_id, project_id, subject_id)?
            .ok_or(ConnectorTaskStoreError::NotFound)?;
        if !matches!(
            task.task_status,
            ConnectorTaskState::ReadyForReview
                | ConnectorTaskState::Accepted
                | ConnectorTaskState::Rejected
        ) || task.run_status != ConnectorRunState::Completed
        {
            return Err(ConnectorTaskStoreError::InvalidState(
                "workspace release can only follow a completed task result".to_string(),
            ));
        }
        if load_result(&tx, task_id)?.is_none() {
            return Err(ConnectorTaskStoreError::InvalidState(
                "workspace release requires a stable task result".to_string(),
            ));
        }
        if let Some(warning) = cleanup_warning {
            tx.execute(
                "UPDATE wc_task_results
                 SET cleanup_warning = CASE
                     WHEN cleanup_warning IS NULL THEN ?1
                     ELSE cleanup_warning || '; ' || ?1
                 END
                 WHERE task_id = ?2",
                params![warning, task_id],
            )?;
        } else if released {
            tx.execute(
                "UPDATE wc_task_results SET cleanup_warning = NULL WHERE task_id = ?1",
                params![task_id],
            )?;
        }
        let sequence = task.event_cursor + 1;
        insert_event(
            &tx,
            task_id,
            &task.run_id,
            sequence,
            "workspace_release",
            &serde_json::json!({
                "released": released,
                "cleanup_warning": cleanup_warning
            }),
            now,
        )?;
        touch_task(&tx, task_id, now)?;
        tx.commit()?;
        Ok(sequence)
    }

    /// Deliver-once watermark for human guidance attached to capability
    /// responses. Reads/advances are scoped like every other task accessor.
    ///
    /// Claim the guidance a task has not yet delivered, advancing the
    /// watermark in the same transaction.
    ///
    /// Reading the watermark, selecting events, and advancing it used to be
    /// three separate statements over a generic "last 50 events" query. Two
    /// concurrent capability responses could therefore both read the same
    /// watermark and deliver the same guidance twice, and guidance older than
    /// fifty events fell out of the window and was never delivered at all.
    ///
    /// One transaction, one query scoped to `human_guidance`, so neither is
    /// possible: the second claimer sees the advanced watermark, and unrelated
    /// event volume cannot push guidance out of view.
    pub fn claim_pending_connector_guidance(
        &self,
        task_id: &str,
        project_id: &str,
        subject_id: &str,
        max_guidance: usize,
    ) -> Result<Vec<ConnectorTaskEvent>, ConnectorTaskStoreError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction()
            .map_err(|error| ConnectorTaskStoreError::Storage(error.into()))?;
        if load_task(&tx, task_id, project_id, subject_id)?.is_none() {
            return Err(ConnectorTaskStoreError::NotFound);
        }
        let seen: i64 = tx
            .query_row(
                "SELECT guidance_seen_seq FROM wc_tasks WHERE id = ?1 AND project_id = ?2",
                rusqlite::params![task_id, project_id],
                |row| row.get(0),
            )
            .map_err(|error| ConnectorTaskStoreError::Storage(error.into()))?;

        let claimed = {
            let mut statement = tx
                .prepare(
                    "SELECT id, sequence, kind, payload_json, created_at
                     FROM wc_task_events
                     WHERE task_id = ?1 AND kind = 'human_guidance' AND sequence > ?2
                     ORDER BY sequence ASC
                     LIMIT ?3",
                )
                .map_err(|error| ConnectorTaskStoreError::Storage(error.into()))?;
            let rows = statement
                .query_map(
                    rusqlite::params![task_id, seen, max_guidance as i64],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, i64>(4)?,
                        ))
                    },
                )
                .map_err(|error| ConnectorTaskStoreError::Storage(error.into()))?;
            let mut claimed = Vec::new();
            for row in rows {
                let (event_id, sequence, kind, payload_json, created_at) =
                    row.map_err(|error| ConnectorTaskStoreError::Storage(error.into()))?;
                claimed.push(ConnectorTaskEvent {
                    event_id,
                    sequence,
                    kind,
                    payload: serde_json::from_str(&payload_json).map_err(|error| {
                        ConnectorTaskStoreError::Storage(anyhow::Error::from(error))
                    })?,
                    created_at,
                });
            }
            claimed
        };

        // Nothing to deliver leaves the watermark alone, so a later claim still
        // sees guidance recorded in the meantime.
        if let Some(max_seq) = claimed.iter().map(|event| event.sequence).max() {
            tx.execute(
                "UPDATE wc_tasks SET guidance_seen_seq = MAX(guidance_seen_seq, ?3)
                 WHERE id = ?1 AND project_id = ?2",
                rusqlite::params![task_id, project_id, max_seq],
            )
            .map_err(|error| ConnectorTaskStoreError::Storage(error.into()))?;
        }
        tx.commit()
            .map_err(|error| ConnectorTaskStoreError::Storage(error.into()))?;
        Ok(claimed)
    }

    /// Read-only guidance read-state for a host review console: the watermark
    /// the model has claimed up to, and the most recent `human_guidance`
    /// sequence still pending (above the watermark). Unlike
    /// [`claim_pending_connector_guidance`], this never advances the watermark
    /// — a host opening the review page must not consume guidance the model
    /// has not yet read. Returns `None` when the task does not exist.
    pub fn connector_guidance_read_state(
        &self,
        task_id: &str,
        project_id: &str,
    ) -> Result<Option<GuidanceReadState>, ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        let exists = conn
            .query_row(
                "SELECT 1 FROM wc_tasks WHERE id = ?1 AND project_id = ?2",
                params![task_id, project_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            return Ok(None);
        }
        let seen: i64 = conn
            .query_row(
                "SELECT guidance_seen_seq FROM wc_tasks WHERE id = ?1 AND project_id = ?2",
                params![task_id, project_id],
                |row| row.get(0),
            )
            .map_err(|error| ConnectorTaskStoreError::Storage(error.into()))?;
        let last_pending: Option<i64> = conn
            .query_row(
                "SELECT MAX(sequence) FROM wc_task_events
                 WHERE task_id = ?1 AND kind = 'human_guidance' AND sequence > ?2",
                params![task_id, seen],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| ConnectorTaskStoreError::Storage(error.into()))?
            .flatten();
        Ok(Some(GuidanceReadState {
            seen_seq: seen,
            last_pending_seq: last_pending,
        }))
    }

    /// Every path this task has actually applied, in first-seen order.
    ///
    /// Scoped to `edits_apply` in SQL rather than filtered out of the recent
    /// timeline, so a path applied early in a long task is still reported. The
    /// caller gets the total alongside a bounded list and must say which it is
    /// showing — a truncated list must never be presented as complete.
    pub fn connector_task_applied_paths(
        &self,
        task_id: &str,
        project_id: &str,
        subject_id: &str,
        cap: usize,
    ) -> Result<AppliedPaths, ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        if load_task(&conn, task_id, project_id, subject_id)?.is_none() {
            return Err(ConnectorTaskStoreError::NotFound);
        }
        let mut statement = conn
            .prepare(
                "SELECT payload_json FROM wc_task_events
                 WHERE task_id = ?1 AND kind = 'edits_apply'
                 ORDER BY sequence ASC",
            )
            .map_err(|error| ConnectorTaskStoreError::Storage(error.into()))?;
        let rows = statement
            .query_map(rusqlite::params![task_id], |row| row.get::<_, String>(0))
            .map_err(|error| ConnectorTaskStoreError::Storage(error.into()))?;

        let mut paths: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        for row in rows {
            let payload = row.map_err(|error| ConnectorTaskStoreError::Storage(error.into()))?;
            let Ok(payload) = serde_json::from_str::<serde_json::Value>(&payload) else {
                continue;
            };
            if payload["ok"] != true || payload["dry_run"] == true {
                continue;
            }
            let Some(list) = payload["changed_paths"].as_array() else {
                continue;
            };
            for path in list.iter().filter_map(serde_json::Value::as_str) {
                if !seen.insert(path.to_string()) {
                    continue;
                }
                if paths.len() < cap {
                    paths.push(path.to_string());
                }
            }
        }
        let total = seen.len();
        Ok(AppliedPaths {
            complete: paths.len() == total,
            paths,
            total,
        })
    }

    pub fn connector_task_events(
        &self,
        task_id: &str,
        project_id: &str,
        subject_id: &str,
        limit: usize,
    ) -> Result<Vec<ConnectorTaskEvent>, ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        if load_task(&conn, task_id, project_id, subject_id)?.is_none() {
            return Err(ConnectorTaskStoreError::NotFound);
        }
        let mut statement = conn.prepare(
            "SELECT id, sequence, kind, payload_json, created_at
             FROM wc_task_events
             WHERE task_id = ?1
             ORDER BY sequence DESC
             LIMIT ?2",
        )?;
        let rows = statement.query_map(params![task_id, limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (event_id, sequence, kind, payload_json, created_at) = row?;
            events.push(ConnectorTaskEvent {
                event_id,
                sequence,
                kind,
                payload: serde_json::from_str(&payload_json)?,
                created_at,
            });
        }
        events.reverse();
        Ok(events)
    }

    pub fn connector_task_result(
        &self,
        task_id: &str,
        project_id: &str,
        subject_id: &str,
    ) -> Result<Option<ConnectorTaskResult>, ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        if load_task(&conn, task_id, project_id, subject_id)?.is_none() {
            return Err(ConnectorTaskStoreError::NotFound);
        }
        load_result(&conn, task_id)
    }

    pub fn connector_preserved_workspaces(
        &self,
        project_id: &str,
    ) -> Result<Vec<ConnectorPreservedWorkspace>, ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT t.id, r.id, ctx.execution_root, ctx.execution_executor_ref,
                    ctx.baseline_commit
             FROM wc_tasks t
             JOIN wc_runs r ON r.task_id = t.id
             JOIN wc_run_contexts ctx ON ctx.run_id = r.id
             WHERE t.project_id = ?1 AND r.status = 'interrupted' AND ctx.isolated = 1
               AND NOT EXISTS (
                   SELECT 1 FROM wc_task_events cancelled
                   WHERE cancelled.task_id = t.id AND cancelled.kind = 'task_cancelled'
               )
             ORDER BY r.started_at ASC",
        )?;
        let rows = statement.query_map(params![project_id], |row| {
            Ok(ConnectorPreservedWorkspace {
                task_id: row.get(0)?,
                run_id: row.get(1)?,
                execution_root: row.get(2)?,
                execution_executor_ref: row.get(3)?,
                baseline_commit: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn request_or_consume_connector_approval(
        &self,
        task_id: &str,
        project_id: &str,
        subject_id: &str,
        action_kind: &str,
        action_hash: &str,
        action_summary: &str,
        now: i64,
        expires_at: i64,
    ) -> Result<ConnectorApprovalGate, ConnectorTaskStoreError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let task = load_task(&tx, task_id, project_id, subject_id)?
            .ok_or(ConnectorTaskStoreError::NotFound)?;
        require_running(&task)?;
        let mut approval = load_approval_by_hash(&tx, task_id, &task.run_id, action_hash)?;
        if approval.is_none() {
            let approval_id = new_id("wc_apr");
            tx.execute(
                "INSERT INTO wc_approvals
                    (id, task_id, run_id, action_kind, action_hash, action_summary, state,
                     requested_at, expires_at, decided_by, decided_at, consumed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8, NULL, NULL, NULL)",
                params![
                    approval_id,
                    task_id,
                    task.run_id,
                    action_kind,
                    action_hash,
                    action_summary,
                    now,
                    expires_at
                ],
            )?;
            insert_event(
                &tx,
                task_id,
                &task.run_id,
                task.event_cursor + 1,
                "approval_requested",
                &serde_json::json!({
                    "approval_id": approval_id,
                    "action_kind": action_kind,
                    "action_hash": action_hash,
                    "expires_at": expires_at
                }),
                now,
            )?;
            touch_task(&tx, task_id, now)?;
            approval = load_approval_by_hash(&tx, task_id, &task.run_id, action_hash)?;
        }
        let mut approval = approval.expect("approval inserted or loaded");
        let gate = match approval.state {
            ConnectorApprovalState::Pending if approval.expires_at <= now => {
                tx.execute(
                    "UPDATE wc_approvals SET state = 'expired' WHERE id = ?1 AND state = 'pending'",
                    params![approval.approval_id],
                )?;
                approval.state = ConnectorApprovalState::Expired;
                ConnectorApprovalGate::Expired(approval)
            }
            ConnectorApprovalState::Pending => ConnectorApprovalGate::Pending(approval),
            ConnectorApprovalState::Approved if approval.expires_at <= now => {
                tx.execute(
                    "UPDATE wc_approvals SET state = 'expired' WHERE id = ?1 AND state = 'approved'",
                    params![approval.approval_id],
                )?;
                approval.state = ConnectorApprovalState::Expired;
                ConnectorApprovalGate::Expired(approval)
            }
            ConnectorApprovalState::Approved => {
                let updated = tx.execute(
                    "UPDATE wc_approvals SET state = 'consumed', consumed_at = ?1
                     WHERE id = ?2 AND state = 'approved'",
                    params![now, approval.approval_id],
                )?;
                if updated != 1 {
                    return Err(ConnectorTaskStoreError::InvalidState(
                        "approval was already consumed".to_string(),
                    ));
                }
                approval.state = ConnectorApprovalState::Consumed;
                approval.consumed_at = Some(now);
                insert_event(
                    &tx,
                    task_id,
                    &task.run_id,
                    task.event_cursor + 1,
                    "approval_consumed",
                    &serde_json::json!({
                        "approval_id": approval.approval_id,
                        "action_hash": approval.action_hash
                    }),
                    now,
                )?;
                touch_task(&tx, task_id, now)?;
                ConnectorApprovalGate::Authorized(approval)
            }
            ConnectorApprovalState::Denied => ConnectorApprovalGate::Denied(approval),
            ConnectorApprovalState::Expired => {
                tx.execute(
                    "UPDATE wc_approvals
                     SET state = 'pending', requested_at = ?1, expires_at = ?2,
                         decided_by = NULL, decided_at = NULL, consumed_at = NULL
                     WHERE id = ?3 AND state = 'expired'",
                    params![now, expires_at, approval.approval_id],
                )?;
                insert_event(
                    &tx,
                    task_id,
                    &task.run_id,
                    task.event_cursor + 1,
                    "approval_requested",
                    &serde_json::json!({
                        "approval_id": approval.approval_id,
                        "action_kind": action_kind,
                        "action_hash": action_hash,
                        "expires_at": expires_at,
                        "renewed": true
                    }),
                    now,
                )?;
                touch_task(&tx, task_id, now)?;
                approval.state = ConnectorApprovalState::Pending;
                approval.requested_at = now;
                approval.expires_at = expires_at;
                approval.decided_by = None;
                approval.decided_at = None;
                approval.consumed_at = None;
                ConnectorApprovalGate::Pending(approval)
            }
            ConnectorApprovalState::Consumed => ConnectorApprovalGate::Consumed(approval),
        };
        tx.commit()?;
        Ok(gate)
    }

    pub fn local_connector_tasks(
        &self,
        project_id: &str,
        limit: usize,
    ) -> Result<Vec<ConnectorTaskSnapshot>, ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        let pairs = {
            let mut statement = conn.prepare(
                "SELECT id, owner_subject_id FROM wc_tasks
                 WHERE project_id = ?1 ORDER BY updated_at DESC LIMIT ?2",
            )?;
            let rows = statement.query_map(params![project_id, limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut tasks = Vec::with_capacity(pairs.len());
        for (task_id, subject_id) in pairs {
            if let Some(task) = load_task(&conn, &task_id, project_id, &subject_id)? {
                tasks.push(task);
            }
        }
        Ok(tasks)
    }

    pub fn local_reviewable_tasks(
        &self,
        project_id: &str,
        include_completed: bool,
        limit: usize,
    ) -> Result<Vec<LocalReviewableTask>, ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT q.task_id, q.goal, q.task_status, q.updated_at, q.execution_status,
                    json_extract(q.validation_json, '$.status'),
                    CASE
                        WHEN q.task_status IN ('accepted', 'rejected', 'cancelled') THEN 'closed'
                        WHEN q.run_status = 'interrupted' THEN 'resume_or_reject'
                        WHEN q.task_status = 'ready_for_review' AND q.result_id IS NOT NULL THEN 'review_and_accept'
                        WHEN q.task_status = 'active' THEN 'in_progress'
                        ELSE 'review'
                    END,
                    q.unread_guidance,
                    q.task_lifecycle, q.run_lifecycle, q.result_decision_status, q.cancelled
             FROM (
                SELECT t.id AS task_id, t.goal, t.updated_at,
                    CASE
                        WHEN EXISTS (SELECT 1 FROM wc_task_events c
                                     WHERE c.task_id = t.id AND c.kind = 'task_cancelled') THEN 'cancelled'
                        WHEN res.decision_status = 'accepted' THEN 'accepted'
                        WHEN res.decision_status = 'rejected' THEN 'rejected'
                        WHEN r.status = 'interrupted' THEN 'needs_attention'
                        ELSE t.status
                    END AS task_status,
                    t.status AS task_lifecycle,
                    r.status AS run_lifecycle,
                    res.decision_status AS result_decision_status,
                    EXISTS (SELECT 1 FROM wc_task_events c
                            WHERE c.task_id = t.id AND c.kind = 'task_cancelled') AS cancelled,
                    r.status AS run_status, res.id AS result_id, res.validation_json,
                    (SELECT ex.state FROM wc_executions ex WHERE ex.task_id = t.id
                     ORDER BY ex.submitted_at DESC, ex.rowid DESC LIMIT 1) AS execution_status,
                    (SELECT COUNT(*) FROM wc_task_events g
                     WHERE g.task_id = t.id AND g.kind = 'human_guidance'
                       AND g.sequence > t.guidance_seen_seq) AS unread_guidance
                FROM wc_tasks t
                JOIN wc_runs r ON r.task_id = t.id
                    AND r.started_at = (SELECT MAX(started_at) FROM wc_runs WHERE task_id = t.id)
                LEFT JOIN wc_task_results res ON res.run_id = r.id
                WHERE t.project_id = ?1
             ) q
             WHERE ?2 = 1 OR q.task_status IN ('active', 'needs_attention', 'ready_for_review')
             ORDER BY
                CASE q.task_status
                    WHEN 'needs_attention' THEN 0
                    WHEN 'active' THEN 1
                    WHEN 'ready_for_review' THEN 2
                    ELSE 3
                END,
                q.updated_at DESC,
                q.task_id ASC
             LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![project_id, include_completed as i64, limit.max(1) as i64],
            map_reviewable_task,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The durable tasks one credential may continue from a fresh chat
    /// session: everything [`Self::local_reviewable_tasks`] computes, but
    /// scoped to the owning subject and always including closed history — a
    /// new session often needs the context of an already-decided task.
    pub fn connector_tasks_for_subject(
        &self,
        project_id: &str,
        subject_id: &str,
        limit: usize,
    ) -> Result<Vec<LocalReviewableTask>, ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT q.task_id, q.goal, q.task_status, q.updated_at, q.execution_status,
                    json_extract(q.validation_json, '$.status'),
                    CASE
                        WHEN q.task_status IN ('accepted', 'rejected', 'cancelled') THEN 'closed'
                        WHEN q.run_status = 'interrupted' THEN 'resume_or_reject'
                        WHEN q.task_status = 'ready_for_review' AND q.result_id IS NOT NULL THEN 'review_and_accept'
                        WHEN q.task_status = 'active' THEN 'in_progress'
                        ELSE 'review'
                    END,
                    q.unread_guidance,
                    q.task_lifecycle, q.run_lifecycle, q.result_decision_status, q.cancelled
             FROM (
                SELECT t.id AS task_id, t.goal, t.updated_at,
                    CASE
                        WHEN EXISTS (SELECT 1 FROM wc_task_events c
                                     WHERE c.task_id = t.id AND c.kind = 'task_cancelled') THEN 'cancelled'
                        WHEN res.decision_status = 'accepted' THEN 'accepted'
                        WHEN res.decision_status = 'rejected' THEN 'rejected'
                        WHEN r.status = 'interrupted' THEN 'needs_attention'
                        ELSE t.status
                    END AS task_status,
                    t.status AS task_lifecycle,
                    r.status AS run_lifecycle,
                    res.decision_status AS result_decision_status,
                    EXISTS (SELECT 1 FROM wc_task_events c
                            WHERE c.task_id = t.id AND c.kind = 'task_cancelled') AS cancelled,
                    r.status AS run_status, res.id AS result_id, res.validation_json,
                    (SELECT ex.state FROM wc_executions ex WHERE ex.task_id = t.id
                     ORDER BY ex.submitted_at DESC, ex.rowid DESC LIMIT 1) AS execution_status,
                    (SELECT COUNT(*) FROM wc_task_events g
                     WHERE g.task_id = t.id AND g.kind = 'human_guidance'
                       AND g.sequence > t.guidance_seen_seq) AS unread_guidance
                FROM wc_tasks t
                JOIN wc_runs r ON r.task_id = t.id
                    AND r.started_at = (SELECT MAX(started_at) FROM wc_runs WHERE task_id = t.id)
                LEFT JOIN wc_task_results res ON res.run_id = r.id
                WHERE t.project_id = ?1 AND t.owner_subject_id = ?2
             ) q
             ORDER BY
                CASE q.task_status
                    WHEN 'needs_attention' THEN 0
                    WHEN 'active' THEN 1
                    WHEN 'ready_for_review' THEN 2
                    ELSE 3
                END,
                q.updated_at DESC,
                q.task_id ASC
             LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![project_id, subject_id, limit.max(1) as i64],
            map_reviewable_task,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn local_connector_task(
        &self,
        task_id: &str,
        project_id: &str,
    ) -> Result<ConnectorTaskSnapshot, ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        let subject_id = conn
            .query_row(
                "SELECT owner_subject_id FROM wc_tasks WHERE id = ?1 AND project_id = ?2",
                params![task_id, project_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(ConnectorTaskStoreError::NotFound)?;
        load_task(&conn, task_id, project_id, &subject_id)?.ok_or(ConnectorTaskStoreError::NotFound)
    }

    pub fn local_connector_task_result(
        &self,
        task_id: &str,
        project_id: &str,
    ) -> Result<Option<ConnectorTaskResult>, ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        let exists = conn
            .query_row(
                "SELECT 1 FROM wc_tasks WHERE id = ?1 AND project_id = ?2",
                params![task_id, project_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            return Err(ConnectorTaskStoreError::NotFound);
        }
        load_result(&conn, task_id)
    }

    pub fn local_connector_task_events(
        &self,
        task_id: &str,
        project_id: &str,
        limit: usize,
    ) -> Result<Vec<ConnectorTaskEvent>, ConnectorTaskStoreError> {
        let task = self.local_connector_task(task_id, project_id)?;
        self.connector_task_events(task_id, project_id, &task.owner_subject_id, limit)
    }

    pub fn local_connector_task_approvals(
        &self,
        task_id: &str,
        project_id: &str,
    ) -> Result<Vec<ConnectorApproval>, ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        let exists = conn
            .query_row(
                "SELECT 1 FROM wc_tasks WHERE id = ?1 AND project_id = ?2",
                params![task_id, project_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            return Err(ConnectorTaskStoreError::NotFound);
        }
        let mut statement = conn.prepare(&format!(
            "SELECT {APPROVAL_COLUMNS} FROM wc_approvals
             WHERE task_id = ?1 ORDER BY requested_at DESC"
        ))?;
        let rows = statement.query_map(params![task_id], map_approval)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Pending, unexpired approvals across the whole project, newest first,
    /// each with its task goal for the console approvals panel.
    pub fn local_pending_connector_approvals(
        &self,
        project_id: &str,
        now: i64,
    ) -> Result<Vec<(ConnectorApproval, String)>, ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT a.id, a.task_id, a.run_id, a.action_kind, a.action_hash, a.action_summary,
                    a.state, a.requested_at, a.expires_at, a.decided_by, a.decided_at,
                    a.consumed_at, a.decision_reason, t.goal
             FROM wc_approvals a
             JOIN wc_tasks t ON t.id = a.task_id
             WHERE t.project_id = ?1 AND a.state = 'pending' AND a.expires_at > ?2
             ORDER BY a.requested_at DESC",
        )?;
        let rows = statement.query_map(params![project_id, now], |row| {
            Ok((map_approval(row)?, row.get::<_, String>(13)?))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn resume_connector_task(
        &self,
        task_id: &str,
        project_id: &str,
        actor: &str,
        now: i64,
    ) -> Result<ConnectorTaskSnapshot, ConnectorTaskStoreError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let subject_id = tx
            .query_row(
                "SELECT owner_subject_id FROM wc_tasks WHERE id = ?1 AND project_id = ?2",
                params![task_id, project_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(ConnectorTaskStoreError::NotFound)?;
        let task = load_task(&tx, task_id, project_id, &subject_id)?
            .ok_or(ConnectorTaskStoreError::NotFound)?;
        if task.mode == ConnectorTaskMode::InspectLegacy {
            return Err(ConnectorTaskStoreError::InvalidState(
                "inspect_mode_retired: this pre-0.4 inspect task can no longer execute; reject it locally and start a new read_only or normal task"
                    .to_string(),
            ));
        }
        validate_connector_task_workspace_shape(
            task.mode,
            task.isolated,
            &task.target_root,
            &task.execution_root,
            task.baseline_commit.as_deref(),
            task.baseline_tree.as_deref(),
        )?;
        if task.run_status != ConnectorRunState::Interrupted
            || task.task_status != ConnectorTaskState::NeedsAttention
        {
            return Err(ConnectorTaskStoreError::InvalidState(
                "only an interrupted task can be resumed".to_string(),
            ));
        }
        if load_result(&tx, task_id)?.is_some() {
            return Err(ConnectorTaskStoreError::InvalidState(
                "a task with a stable result cannot be resumed".to_string(),
            ));
        }
        tx.execute(
            "UPDATE wc_runs SET status = 'running', finished_at = NULL WHERE id = ?1",
            params![task.run_id],
        )?;
        insert_event(
            &tx,
            task_id,
            &task.run_id,
            task.event_cursor + 1,
            "run_resumed",
            &serde_json::json!({ "actor": actor }),
            now,
        )?;
        touch_task(&tx, task_id, now)?;
        tx.commit()?;
        load_task(&conn, task_id, project_id, &subject_id)?.ok_or(ConnectorTaskStoreError::NotFound)
    }

    pub fn abandon_interrupted_connector_task(
        &self,
        task_id: &str,
        project_id: &str,
        actor: &str,
        now: i64,
    ) -> Result<ConnectorTaskResult, ConnectorTaskStoreError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let (run_id, cursor) = tx
            .query_row(
                "SELECT r.id, COALESCE(MAX(e.sequence), 0)
                 FROM wc_tasks t
                 JOIN wc_runs r ON r.task_id = t.id
                 LEFT JOIN wc_task_events e ON e.task_id = t.id
                 LEFT JOIN wc_task_results result ON result.task_id = t.id
                 WHERE t.id = ?1 AND t.project_id = ?2
                   AND r.status = 'interrupted' AND result.id IS NULL
                 GROUP BY r.id ORDER BY r.started_at DESC LIMIT 1",
                params![task_id, project_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .ok_or_else(|| {
                ConnectorTaskStoreError::InvalidState(
                    "only an interrupted task without a Result can be abandoned".to_string(),
                )
            })?;
        let result_id = new_id("wc_result");
        let warnings = vec!["interrupted workspace changes were discarded locally"];
        tx.execute(
            "INSERT INTO wc_task_results
                (id, task_id, run_id, summary, patch_artifact, patch_sha256, patch_bytes,
                 changed_paths_json, validation_json, warnings_json, decision_status,
                 decided_by, decided_at, cleanup_warning, created_at)
             VALUES (?1, ?2, ?3, ?4, NULL, NULL, 0, '[]', ?5, ?6, 'rejected',
                     ?7, ?8, NULL, ?8)",
            params![
                result_id,
                task_id,
                run_id,
                "Interrupted task abandoned locally without capturing a patch.",
                serde_json::to_string(&serde_json::json!({"status": "not_run"}))?,
                serde_json::to_string(&warnings)?,
                actor,
                now
            ],
        )?;
        insert_event(
            &tx,
            task_id,
            &run_id,
            cursor + 1,
            "task_abandoned",
            &serde_json::json!({
                "actor": actor,
                "result_id": result_id,
                "changes_captured": false
            }),
            now,
        )?;
        tx.execute(
            "UPDATE wc_runs SET status = 'completed', finished_at = ?1 WHERE id = ?2",
            params![now, run_id],
        )?;
        tx.execute(
            "UPDATE wc_tasks SET status = 'ready_for_review', updated_at = ?1 WHERE id = ?2",
            params![now, task_id],
        )?;
        expire_task_approvals(&tx, task_id)?;
        tx.commit()?;
        load_result(&conn, task_id)?
            .ok_or_else(|| ConnectorTaskStoreError::Storage(anyhow::anyhow!("result disappeared")))
    }

    pub fn begin_connector_result_decision(
        &self,
        task_id: &str,
        project_id: &str,
        result_id: &str,
        decision: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), ConnectorTaskStoreError> {
        let decision = ConnectorResultDecision::requested(decision)?;
        let conn = self.conn.lock().unwrap();
        let inserted = conn.execute(
            "INSERT INTO wc_result_decision_intents
                (task_id, result_id, decision, actor, started_at)
             SELECT task.id, result.id, ?3, ?4, ?5
             FROM wc_tasks task JOIN wc_task_results result ON result.task_id = task.id
             WHERE task.id = ?1 AND task.project_id = ?2
               AND result.id = ?6 AND result.decision_status = 'pending'
             ON CONFLICT(task_id) DO UPDATE SET
                 decision = excluded.decision,
                 actor = excluded.actor,
                 started_at = excluded.started_at,
                 state = 'pending',
                 error_code = NULL,
                 error_message = NULL,
                 last_attempt_at = NULL
             WHERE wc_result_decision_intents.result_id = excluded.result_id
               AND wc_result_decision_intents.state = 'needs_attention'
               AND excluded.decision = 'rejected'",
            params![task_id, project_id, decision.as_db(), actor, now, result_id],
        )?;
        if inserted == 1 {
            return Ok(());
        }
        Err(ConnectorTaskStoreError::decision(
            "result_decision_in_progress",
            "another result decision won the durable intent race",
        ))
    }

    pub fn abort_connector_result_decision(
        &self,
        task_id: &str,
        result_id: &str,
    ) -> Result<(), ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM wc_result_decision_intents WHERE task_id = ?1 AND result_id = ?2",
            params![task_id, result_id],
        )?;
        Ok(())
    }

    pub fn connector_result_decision_intents(
        &self,
        project_id: &str,
    ) -> Result<Vec<(String, String, ConnectorResultDecision)>, ConnectorTaskStoreError> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT i.task_id, i.result_id, i.decision
             FROM wc_result_decision_intents i
             JOIN wc_tasks t ON t.id = i.task_id
             WHERE t.project_id = ?1 AND i.state = 'pending'
             ORDER BY i.started_at, i.task_id",
        )?;
        let rows = statement.query_map([project_id], |row| {
            let decision = row.get::<_, String>(2)?;
            Ok((
                row.get(0)?,
                row.get(1)?,
                ConnectorResultDecision::from_db(&decision, 2)?,
            ))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn mark_connector_result_decision_needs_attention(
        &self,
        task_id: &str,
        project_id: &str,
        result_id: &str,
        error_code: &str,
        error_message: &str,
        now: i64,
    ) -> Result<(), ConnectorTaskStoreError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let (run_id, cursor) = tx
            .query_row(
                "SELECT r.id, COALESCE(MAX(e.sequence), 0)
                 FROM wc_tasks t
                 JOIN wc_runs r ON r.task_id = t.id
                 LEFT JOIN wc_task_events e ON e.task_id = t.id
                 WHERE t.id = ?1 AND t.project_id = ?2
                 GROUP BY r.id ORDER BY r.started_at DESC LIMIT 1",
                params![task_id, project_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .ok_or(ConnectorTaskStoreError::NotFound)?;
        let updated = tx.execute(
            "UPDATE wc_result_decision_intents
             SET state = 'needs_attention', error_code = ?1, error_message = ?2,
                 last_attempt_at = ?3
             WHERE task_id = ?4 AND result_id = ?5 AND state = 'pending'",
            params![error_code, error_message, now, task_id, result_id],
        )?;
        if updated != 1 {
            return Err(ConnectorTaskStoreError::decision(
                "result_recovery_state_changed",
                "result recovery state changed while it was being quarantined",
            ));
        }
        tx.execute(
            "UPDATE wc_runs
             SET status = 'interrupted', finished_at = COALESCE(finished_at, ?1)
             WHERE id = ?2",
            params![now, run_id],
        )?;
        insert_event(
            &tx,
            task_id,
            &run_id,
            cursor + 1,
            "result_recovery_needs_attention",
            &serde_json::json!({
                "result_id": result_id,
                "error_code": error_code,
                "error_message": error_message
            }),
            now,
        )?;
        touch_task(&tx, task_id, now)?;
        tx.commit()?;
        Ok(())
    }

    pub fn finalize_connector_result_decision(
        &self,
        task_id: &str,
        project_id: &str,
        result_id: &str,
        cleanup_warning: Option<&str>,
        now: i64,
    ) -> Result<ConnectorTaskResult, ConnectorTaskStoreError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let (decision, actor, run_id, cursor) = tx
            .query_row(
                "SELECT i.decision, i.actor, r.id, COALESCE(MAX(e.sequence), 0)
                 FROM wc_result_decision_intents i
                 JOIN wc_tasks t ON t.id = i.task_id
                 JOIN wc_task_results result ON result.task_id = t.id
                 JOIN wc_runs r ON r.id = result.run_id
                 LEFT JOIN wc_task_events e ON e.task_id = t.id
                 WHERE i.task_id = ?1 AND i.result_id = ?2 AND t.project_id = ?3
                   AND i.state = 'pending' AND result.decision_status = 'pending'
                 GROUP BY i.decision, i.actor, r.id
                 ORDER BY r.started_at DESC LIMIT 1",
                params![task_id, result_id, project_id],
                |row| {
                    let decision = row.get::<_, String>(0)?;
                    Ok((
                        ConnectorResultDecision::from_db(&decision, 0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                ConnectorTaskStoreError::decision(
                    "result_already_decided",
                    "task result was already decided",
                )
            })?;
        let updated = tx.execute(
            "UPDATE wc_task_results
             SET decision_status = ?1, decided_by = ?2, decided_at = ?3,
                 cleanup_warning = COALESCE(?4, cleanup_warning)
             WHERE task_id = ?5 AND id = ?6 AND decision_status = 'pending'",
            params![
                decision.as_db(),
                actor,
                now,
                cleanup_warning,
                task_id,
                result_id
            ],
        )?;
        if updated != 1 {
            return Err(ConnectorTaskStoreError::decision(
                "result_already_decided",
                "task result was already decided",
            ));
        }
        tx.execute(
            "UPDATE wc_tasks SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![decision.as_db(), now, task_id],
        )?;
        tx.execute(
            "UPDATE wc_runs
             SET status = 'completed', finished_at = COALESCE(finished_at, ?1)
             WHERE id = ?2",
            params![now, run_id],
        )?;
        insert_event(
            &tx,
            task_id,
            &run_id,
            cursor + 1,
            match decision {
                ConnectorResultDecision::Accepted => "task_accepted",
                ConnectorResultDecision::Rejected => "task_rejected",
            },
            &serde_json::json!({
                "decision": decision,
                "actor": actor,
                "cleanup_warning": cleanup_warning
            }),
            now,
        )?;
        tx.execute(
            "DELETE FROM wc_result_decision_intents WHERE task_id = ?1",
            [task_id],
        )?;
        tx.commit()?;
        load_result(&conn, task_id)?
            .ok_or_else(|| ConnectorTaskStoreError::Storage(anyhow::anyhow!("result disappeared")))
    }

    pub fn decide_connector_approval(
        &self,
        task_id: &str,
        project_id: &str,
        approval_id: &str,
        approve: bool,
        actor: &str,
        reason: Option<&str>,
        now: i64,
    ) -> Result<ConnectorApproval, ConnectorTaskStoreError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let subject_id = tx
            .query_row(
                "SELECT owner_subject_id FROM wc_tasks WHERE id = ?1 AND project_id = ?2",
                params![task_id, project_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(ConnectorTaskStoreError::NotFound)?;
        let task = load_task(&tx, task_id, project_id, &subject_id)?
            .ok_or(ConnectorTaskStoreError::NotFound)?;
        require_running(&task)?;
        let approval =
            load_approval(&tx, approval_id, task_id)?.ok_or(ConnectorTaskStoreError::NotFound)?;
        if approval.state != ConnectorApprovalState::Pending {
            return Err(ConnectorTaskStoreError::InvalidState(format!(
                "approval is {}; only pending approvals can be decided",
                approval.state
            )));
        }
        if approval.expires_at <= now {
            tx.execute(
                "UPDATE wc_approvals SET state = 'expired' WHERE id = ?1",
                params![approval_id],
            )?;
            tx.commit()?;
            return Err(ConnectorTaskStoreError::InvalidState(
                "approval expired; submit the exact action again to request a new decision"
                    .to_string(),
            ));
        }
        let state = if approve {
            ConnectorApprovalState::Approved
        } else {
            ConnectorApprovalState::Denied
        };
        let reason = reason.map(str::trim).filter(|reason| !reason.is_empty());
        tx.execute(
            "UPDATE wc_approvals SET state = ?1, decided_by = ?2, decided_at = ?3,
                    decision_reason = ?5
             WHERE id = ?4 AND state = 'pending'",
            params![state.as_db(), actor, now, approval_id, reason],
        )?;
        let cursor: i64 = tx.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM wc_task_events WHERE task_id = ?1",
            params![task_id],
            |row| row.get(0),
        )?;
        insert_event(
            &tx,
            task_id,
            &approval.run_id,
            cursor + 1,
            if approve {
                "approval_granted"
            } else {
                "approval_denied"
            },
            &serde_json::json!({
                "approval_id": approval_id,
                "action_hash": approval.action_hash,
                "actor": actor,
                "reason": reason
            }),
            now,
        )?;
        touch_task(&tx, task_id, now)?;
        tx.commit()?;
        load_approval(&conn, approval_id, task_id)?.ok_or(ConnectorTaskStoreError::NotFound)
    }
}

fn map_reviewable_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<LocalReviewableTask> {
    let projected_raw = row.get::<_, String>(2)?;
    let task_lifecycle_raw = row.get::<_, String>(8)?;
    let run_lifecycle_raw = row.get::<_, String>(9)?;
    let result_decision_raw = row.get::<_, Option<String>>(10)?;
    let cancelled = row.get::<_, i64>(11)? != 0;
    let task_lifecycle = ConnectorTaskLifecycle::from_db(&task_lifecycle_raw, 8)?;
    let run_lifecycle = ConnectorRunLifecycle::from_db(&run_lifecycle_raw, 9)?;
    let result_decision = result_decision_raw
        .as_deref()
        .map(|value| ConnectorResultDecisionStatus::from_db(value, 10))
        .transpose()?;
    let canonical = effective_task_state(task_lifecycle, run_lifecycle, result_decision, cancelled);
    let projected = ConnectorTaskState::from_projection(&projected_raw, 2)?;
    if projected != canonical {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            2,
            Type::Text,
            format!(
                "Connector Task SQL projection diverged from canonical Rust projection: SQL={projected}, Rust={canonical}"
            )
            .into(),
        ));
    }
    let execution_status = row
        .get::<_, Option<String>>(4)?
        .as_deref()
        .map(|value| ConnectorExecutionState::from_db(value, 4))
        .transpose()?;
    Ok(LocalReviewableTask {
        task_id: row.get(0)?,
        goal: row.get(1)?,
        task_status: canonical,
        updated_at: row.get(3)?,
        execution_status,
        validation_status: row.get(5)?,
        next_action: row.get(6)?,
        unread_guidance: row.get(7)?,
    })
}

pub(super) fn load_task(
    conn: &rusqlite::Connection,
    task_id: &str,
    project_id: &str,
    subject_id: &str,
) -> Result<Option<ConnectorTaskSnapshot>, rusqlite::Error> {
    conn.query_row(
        "SELECT t.id, r.id, t.project_id, r.workspace_id, t.owner_subject_id, t.goal, t.mode,
                t.status, r.status, result.decision_status,
                EXISTS (
                    SELECT 1 FROM wc_task_events cancelled
                    WHERE cancelled.task_id = t.id AND cancelled.kind = 'task_cancelled'
                ),
                COALESCE(MAX(e.sequence), 0),
                ctx.target_executor_ref, ctx.execution_executor_ref,
                ctx.target_root, ctx.execution_root,
                ctx.baseline_commit, ctx.baseline_tree, ctx.isolated,
                t.created_at, t.updated_at
         FROM wc_tasks t
         JOIN wc_runs r ON r.task_id = t.id
         JOIN wc_run_contexts ctx ON ctx.run_id = r.id
         LEFT JOIN wc_task_results result ON result.run_id = r.id
         LEFT JOIN wc_task_events e ON e.task_id = t.id
         WHERE t.id = ?1 AND t.project_id = ?2 AND t.owner_subject_id = ?3
         GROUP BY t.id, r.id
         ORDER BY r.started_at DESC
         LIMIT 1",
        params![task_id, project_id, subject_id],
        |row| {
            let mode_raw = row.get::<_, String>(6)?;
            let task_lifecycle_raw = row.get::<_, String>(7)?;
            let run_lifecycle_raw = row.get::<_, String>(8)?;
            let result_decision_raw = row.get::<_, Option<String>>(9)?;
            let cancelled = row.get::<_, i64>(10)? != 0;
            let mode = ConnectorTaskMode::from_db(&mode_raw, 6)?;
            let task_lifecycle = ConnectorTaskLifecycle::from_db(&task_lifecycle_raw, 7)?;
            let run_lifecycle = ConnectorRunLifecycle::from_db(&run_lifecycle_raw, 8)?;
            let result_decision = result_decision_raw
                .as_deref()
                .map(|value| ConnectorResultDecisionStatus::from_db(value, 9))
                .transpose()?;
            Ok(ConnectorTaskSnapshot {
                task_id: row.get(0)?,
                run_id: row.get(1)?,
                project_id: row.get(2)?,
                workspace_id: row.get(3)?,
                owner_subject_id: row.get(4)?,
                goal: row.get(5)?,
                mode,
                task_lifecycle,
                task_status: effective_task_state(
                    task_lifecycle,
                    run_lifecycle,
                    result_decision,
                    cancelled,
                ),
                run_lifecycle,
                run_status: effective_run_state(run_lifecycle, cancelled),
                event_cursor: row.get(11)?,
                target_executor_ref: row.get(12)?,
                execution_executor_ref: row.get(13)?,
                target_root: row.get(14)?,
                execution_root: row.get(15)?,
                baseline_commit: row.get(16)?,
                baseline_tree: row.get(17)?,
                isolated: row.get::<_, i64>(18)? != 0,
                created_at: row.get(19)?,
                updated_at: row.get(20)?,
            })
        },
    )
    .optional()
}

fn load_result(
    conn: &rusqlite::Connection,
    task_id: &str,
) -> Result<Option<ConnectorTaskResult>, ConnectorTaskStoreError> {
    conn.query_row(
        "SELECT result.id, result.task_id, result.run_id, result.summary,
                result.patch_artifact, result.patch_sha256, result.patch_bytes,
                result.changed_paths_json, result.validation_json, result.warnings_json,
                result.decision_status, result.decided_by, result.decided_at,
                result.cleanup_warning, result.created_at,
                intent.state, intent.decision, intent.error_code, intent.error_message,
                intent.last_attempt_at
         FROM wc_task_results result
         LEFT JOIN wc_result_decision_intents intent
           ON intent.task_id = result.task_id AND intent.result_id = result.id
         WHERE result.task_id = ?1",
        params![task_id],
        map_result,
    )
    .optional()
    .map_err(ConnectorTaskStoreError::from)
}

fn map_result(row: &rusqlite::Row<'_>) -> Result<ConnectorTaskResult, rusqlite::Error> {
    fn json_col<T: serde::de::DeserializeOwned>(
        row: &rusqlite::Row<'_>,
        idx: usize,
    ) -> Result<T, rusqlite::Error> {
        let raw: String = row.get(idx)?;
        serde_json::from_str(&raw)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e)))
    }
    let patch_bytes_raw: i64 = row.get(6)?;
    let patch_bytes = usize::try_from(patch_bytes_raw)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(6, Type::Integer, Box::new(e)))?;
    let recovery = match row.get::<_, Option<String>>(15)? {
        Some(state) => {
            let decision = row.get::<_, String>(16)?;
            Some(ConnectorResultDecisionRecovery {
                state: ConnectorResultDecisionRecoveryState::from_db(&state, 15)?,
                decision: ConnectorResultDecision::from_db(&decision, 16)?,
                error_code: row.get(17)?,
                error_message: row.get(18)?,
                last_attempt_at: row.get(19)?,
            })
        }
        None => None,
    };
    Ok(ConnectorTaskResult {
        result_id: row.get(0)?,
        task_id: row.get(1)?,
        run_id: row.get(2)?,
        summary: row.get(3)?,
        patch_artifact: row.get(4)?,
        patch_sha256: row.get(5)?,
        patch_bytes,
        changed_paths: json_col(row, 7)?,
        validation: json_col(row, 8)?,
        warnings: json_col(row, 9)?,
        decision_status: ConnectorResultDecisionStatus::from_db(&row.get::<_, String>(10)?, 10)?,
        decided_by: row.get(11)?,
        decided_at: row.get(12)?,
        cleanup_warning: row.get(13)?,
        recovery,
        created_at: row.get(14)?,
    })
}

fn load_approval_by_hash(
    conn: &rusqlite::Connection,
    task_id: &str,
    run_id: &str,
    action_hash: &str,
) -> Result<Option<ConnectorApproval>, rusqlite::Error> {
    conn.query_row(
        &format!(
            "SELECT {APPROVAL_COLUMNS} FROM wc_approvals
             WHERE task_id = ?1 AND run_id = ?2 AND action_hash = ?3"
        ),
        params![task_id, run_id, action_hash],
        map_approval,
    )
    .optional()
}

fn load_approval(
    conn: &rusqlite::Connection,
    approval_id: &str,
    task_id: &str,
) -> Result<Option<ConnectorApproval>, rusqlite::Error> {
    conn.query_row(
        &format!("SELECT {APPROVAL_COLUMNS} FROM wc_approvals WHERE id = ?1 AND task_id = ?2"),
        params![approval_id, task_id],
        map_approval,
    )
    .optional()
}

/// Column list backing every `wc_approvals` read that feeds `map_approval`.
/// Order must match `map_approval`'s positional `row.get` indices.
const APPROVAL_COLUMNS: &str = "id, task_id, run_id, action_kind, action_hash, action_summary, \
     state, requested_at, expires_at, decided_by, decided_at, consumed_at, decision_reason";

fn map_approval(row: &rusqlite::Row<'_>) -> Result<ConnectorApproval, rusqlite::Error> {
    Ok(ConnectorApproval {
        approval_id: row.get(0)?,
        task_id: row.get(1)?,
        run_id: row.get(2)?,
        action_kind: row.get(3)?,
        action_hash: row.get(4)?,
        action_summary: row.get(5)?,
        state: ConnectorApprovalState::from_db(&row.get::<_, String>(6)?, 6)?,
        requested_at: row.get(7)?,
        expires_at: row.get(8)?,
        decided_by: row.get(9)?,
        decided_at: row.get(10)?,
        consumed_at: row.get(11)?,
        decision_reason: row.get(12)?,
    })
}

/// Bump `wc_tasks.updated_at`. Accepts anything that derefs to a `Connection`
/// (both `&Transaction` and `&Connection` work).
pub(super) fn touch_task(
    conn: &rusqlite::Connection,
    task_id: &str,
    now: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE wc_tasks SET updated_at = ?1 WHERE id = ?2",
        params![now, task_id],
    )
}

/// Expire every still-open approval for a task (pending or already approved but
/// not yet consumed). Used on finish / interrupt / abandon.
pub(super) fn expire_task_approvals(
    conn: &rusqlite::Connection,
    task_id: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE wc_approvals SET state = 'expired'
         WHERE task_id = ?1 AND state IN ('pending', 'approved')",
        params![task_id],
    )
}

fn validate_connector_task_workspace_shape(
    mode: ConnectorTaskMode,
    isolated: bool,
    target_root: &str,
    execution_root: &str,
    baseline_commit: Option<&str>,
    baseline_tree: Option<&str>,
) -> Result<(), ConnectorTaskStoreError> {
    match mode {
        ConnectorTaskMode::Normal
            if !isolated
                || execution_root == target_root
                || baseline_commit.is_none_or(str::is_empty)
                || baseline_tree.is_none_or(str::is_empty) =>
        {
            Err(ConnectorTaskStoreError::InvalidState(
                "normal tasks require an isolated execution root and Git baseline".to_string(),
            ))
        }
        ConnectorTaskMode::ReadOnly if isolated || execution_root != target_root => {
            Err(ConnectorTaskStoreError::InvalidState(
                "read_only tasks must use the target workspace without isolation".to_string(),
            ))
        }
        ConnectorTaskMode::Normal | ConnectorTaskMode::ReadOnly => Ok(()),
        ConnectorTaskMode::InspectLegacy => Err(ConnectorTaskStoreError::InvalidState(
            "task mode must be normal or read_only".to_string(),
        )),
    }
}

pub(super) fn require_running(task: &ConnectorTaskSnapshot) -> Result<(), ConnectorTaskStoreError> {
    if task.task_status != ConnectorTaskState::Active
        || task.run_status != ConnectorRunState::Running
    {
        return Err(ConnectorTaskStoreError::InvalidState(format!(
            "task {} is {}, run is {}; start a new task for more work",
            task.task_id, task.task_status, task.run_status
        )));
    }
    Ok(())
}

pub(super) fn insert_event(
    tx: &Transaction<'_>,
    task_id: &str,
    run_id: &str,
    sequence: i64,
    kind: &str,
    payload: &Value,
    now: i64,
) -> Result<(), ConnectorTaskStoreError> {
    tx.execute(
        "INSERT INTO wc_task_events
            (id, task_id, run_id, sequence, kind, payload_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            new_id("wc_evt"),
            task_id,
            run_id,
            sequence,
            kind,
            serde_json::to_string(payload)?,
            now
        ],
    )?;
    Ok(())
}

fn bind_window_context(
    tx: &Transaction<'_>,
    task_id: &str,
    project_id: &str,
    subject_id: &str,
    binding: ConnectorWindowBinding<'_>,
) -> Result<(), ConnectorTaskStoreError> {
    if binding.fingerprint.project_root_sha256 != binding.project_root_sha256 {
        return Err(ConnectorTaskStoreError::InvalidState(
            "window context root identity does not match its fingerprint".to_string(),
        ));
    }
    let fingerprint_json = serde_json::to_string(binding.fingerprint)?;
    let owns_task = tx
        .query_row(
            "SELECT 1 FROM wc_tasks
             WHERE id = ?1 AND project_id = ?2 AND owner_subject_id = ?3",
            params![task_id, project_id, subject_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !owns_task {
        return Err(ConnectorTaskStoreError::NotFound);
    }
    // A task has one controlling window. Explicit recovery therefore moves
    // this lightweight binding rather than cloning active context.
    tx.execute(
        "DELETE FROM wc_window_project_contexts
         WHERE task_id = ?1
           AND NOT (
                window_key = ?2 AND project_id = ?3
                AND owner_subject_id = ?4 AND project_root_sha256 = ?5
           )",
        params![
            task_id,
            binding.window_key,
            project_id,
            subject_id,
            binding.project_root_sha256
        ],
    )?;
    tx.execute(
        "INSERT INTO wc_window_project_contexts
            (window_key, window_source, project_id, owner_subject_id,
             project_root_sha256, task_id, target_path, fingerprint_json,
             created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
         ON CONFLICT(
            window_key,
            project_id,
            owner_subject_id,
            project_root_sha256
         ) DO UPDATE SET
            window_source = excluded.window_source,
            task_id = excluded.task_id,
            target_path = excluded.target_path,
            fingerprint_json = excluded.fingerprint_json,
            updated_at = excluded.updated_at",
        params![
            binding.window_key,
            binding.window_source,
            project_id,
            subject_id,
            binding.project_root_sha256,
            task_id,
            binding.target_path,
            fingerprint_json,
            binding.now
        ],
    )?;
    Ok(())
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

#[cfg(test)]
#[path = "task_kernel_tests.rs"]
mod tests;
