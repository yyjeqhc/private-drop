//! Durable WebCodex state persistence and SQLite storage semantics.

use crate::models::PairingCodeRecord;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

mod accounts;
mod activity;
mod admin_project_lifecycle;
mod agent_attention;
mod agent_task;
mod agent_wake;
mod audit;
mod communication;
mod execution_model;
mod executions;
mod goal;
mod job_receipts;
mod memory;
pub mod models;
mod oauth;
mod schema;
mod server_instance;
mod task_kernel;
mod window_activity;

pub use self::admin_project_lifecycle::{AdminProjectAudit, AdminProjectIdempotencyRecord};
pub use self::agent_task::{
    AgentTaskAttemptCompletionMutation, AgentTaskAttemptHeartbeatMutation, AgentTaskAttemptRecord,
    AgentTaskAttemptStartMutation, AgentTaskAttemptState, AgentTaskCodingRunBindingIntent,
    AgentTaskCodingRunBindingRecord, AgentTaskCodingRunDispatchClaim,
    AgentTaskCodingRunDispatchState, AgentTaskCodingRunObservation, AgentTaskCodingRunPrepared,
    AgentTaskCodingRunReconcileMutation, AgentTaskCodingRunStartContext, AgentTaskDetail,
    AgentTaskExecutionKind, AgentTaskExecutionRecoveryKind, AgentTaskExecutionStatus,
    AgentTaskMutation, AgentTaskPage, AgentTaskState, AgentTaskSummary, NewAgentTask,
    MAX_AGENT_TASK_LIST_LIMIT, MAX_AGENT_TASK_TERMINAL_TEXT_BYTES,
};
#[allow(unused_imports)]
pub use self::agent_wake::{
    AgentConversationBootstrapRecord, AgentInboxBootstrapSummary, AgentWakeAttemptRecord,
    AgentWakeAttemptState, AgentWakeBootstrapSummary, AgentWakeClaim, AgentWakeConsumeResult,
    AgentWakeEnvelope, AgentWakeExplicitActivation, AgentWakePrepared, AgentWakeRecord,
    AgentWakeState, AGENT_WAKE_CONSUME_TOKEN_PREFIX, AGENT_WAKE_ID_PREFIX,
};
pub use self::communication::{
    AgentEndpointLifecycle, AgentEndpointMutation, AgentEndpointRecord, AgentIdentityMutation,
    AgentIdentityPage, AgentInboxItem, AgentInboxPage, AgentProfilePatch, CommunicationPrincipal,
    CommunicationStoreError, ConversationAccess, ConversationDetailRecord, ConversationLifecycle,
    ConversationMessageMutation, ConversationMessageRecord, ConversationMutation, ConversationPage,
    ConversationParticipantRecord, ConversationSummaryRecord, DeliveryConsumeResult,
    DurableAgentIdentity, McpAppEndpointRecovery, MessageAuthorRecord, MessageDeliveryRecord,
    MessageDeliveryState, NewAgentEndpoint, NewAgentIdentity, NewConversation,
    NewConversationMessage, COMMUNICATION_PRINCIPAL_DIGEST_PREFIX, MAX_COMMUNICATION_LIST_LIMIT,
    MAX_DURABLE_AGENTS,
};
pub use self::execution_model::{
    ConnectorExecution, ConnectorExecutionFailure, ConnectorExecutionKind,
    ConnectorExecutionObservation, ConnectorExecutionReservation, ConnectorExecutionState,
    ConnectorTerminalContinuationDeliveryState, MAX_ASSERTION_EVIDENCE_BYTES,
};
#[cfg(any(test, feature = "root-test-support"))]
pub use self::execution_model::{
    ConnectorExecutionContinuationIntent, ConnectorTerminalContinuationClaim,
};
pub use self::goal::{
    GoalCorrelation, GoalCorrelationKind, GoalDetail, GoalLifecycle, GoalMutation, GoalPage,
    GoalPatch, GoalStoreError, GoalSummary, NewGoal, GOAL_ID_PREFIX, MAX_GOAL_CORRELATIONS,
    MAX_GOAL_LIST_LIMIT, MAX_GOAL_OBJECTIVE_BYTES, MAX_GOAL_TERMINAL_REASON_BYTES,
    MAX_GOAL_TITLE_CHARS, WORKFLOW_SESSION_ID_PREFIX,
};
#[allow(unused_imports)]
pub use self::memory::{
    canonicalize_memory_tags, memory_catalog_revision, valid_memory_catalog_revision,
    validate_memory_query, validate_memory_revision, validate_memory_scope_id, MemoryDeleteOutcome,
    MemoryPrincipalAttribution, MemoryPriority, MemoryScopeAttribution, MemoryScopePurgeOutcome,
    MemorySetInput, MemorySetOutcome, MemoryStoreError, ProjectMemoryRecord,
    ProjectMemoryScopeRecord, ProjectMemoryScopeSnapshot, MAX_MEMORIES_GLOBAL,
    MAX_MEMORY_BODY_BYTES, MAX_MEMORY_BOOTSTRAP_BYTES, MAX_MEMORY_KEY_CHARS,
    MAX_MEMORY_QUERY_CHARS, MAX_MEMORY_SCOPE_LIST_LIMIT, MAX_MEMORY_SEARCH_LIMIT,
    MAX_MEMORY_SEARCH_RESULT_BYTES, MAX_MEMORY_SUMMARY_CHARS, MAX_MEMORY_TAGS,
    MAX_MEMORY_TAG_CHARS,
};
#[cfg(any(test, feature = "root-test-support"))]
pub use self::memory::{
    memory_definition_hash, memory_state_revision, validate_memory_body, validate_memory_key,
    validate_memory_summary, MAX_MEMORIES_PER_PROJECT, MEMORY_SCOPE_IDENTITY_ATTRIBUTED,
};
pub use self::oauth::RotateResult;
pub use self::server_instance::ServerInstanceGuard;
pub use self::task_kernel::{
    AppliedPaths, ConnectorApproval, ConnectorApprovalGate, ConnectorApprovalState,
    ConnectorBinding, ConnectorEditOperationGate, ConnectorPreservedWorkspace,
    ConnectorResultDecision, ConnectorResultDecisionRecovery, ConnectorResultDecisionRecoveryState,
    ConnectorResultDecisionStatus, ConnectorRunLifecycle, ConnectorRunState,
    ConnectorTaskContinuation, ConnectorTaskEvent, ConnectorTaskLifecycle, ConnectorTaskMode,
    ConnectorTaskResult, ConnectorTaskSnapshot, ConnectorTaskState, ConnectorTaskStoreError,
    ConnectorWindowBinding, ConnectorWindowContext, ConnectorWorkspaceTransition,
    GuidanceReadState, LocalReviewableTask, NewConnectorResult, NewConnectorTask,
    WindowProjectActivation,
};
pub use self::window_activity::{MAX_WINDOW_ACTIVITY_LIMIT, MAX_WINDOW_LINK_LIMIT};
pub struct Database {
    conn: Mutex<Connection>,
    state_path: PathBuf,
    /// Ephemeral navigation only. Connector work stays in wc_tasks and
    /// wc_window_project_contexts; AgentTask owns separate durable tables, and
    /// restarting never guesses a window's current project.
    window_projects: Mutex<HashMap<(String, String), String>>,
}

impl Database {
    pub(crate) fn state_path(&self) -> &Path {
        &self.state_path
    }
}

#[derive(Debug, Clone)]
pub enum PairingConsumeResult {
    NotFound,
    Consumed(PairingCodeRecord),
    AlreadyUsed(PairingCodeRecord),
    Expired(PairingCodeRecord),
    ClientMismatch(PairingCodeRecord),
}

#[cfg(any(test, feature = "root-test-support"))]
impl Database {
    /// Test-only access to the underlying connection so tests can assert on
    /// raw storage (e.g. that a plaintext token is never stored as `key_hash`).
    pub fn conn_for_tests(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }
}

#[cfg(test)]
mod agent_attention_tests;
#[cfg(test)]
mod agent_task_tests;
#[cfg(test)]
mod agent_wake_recovery_tests;
#[cfg(test)]
mod agent_wake_tests;
#[cfg(test)]
mod communication_tests;
#[cfg(test)]
mod continuation_delivery_tests;
#[cfg(test)]
mod db_tests;
#[cfg(test)]
mod execution_intent_tests;
#[cfg(test)]
mod goal_tests;
#[cfg(test)]
mod memory_tests;

#[cfg(test)]
mod job_receipts_tests;
