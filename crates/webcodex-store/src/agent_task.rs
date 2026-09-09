use super::communication::{
    authorize_conversation_access, digest_text, lookup_idempotent_resource, new_id, now_unix_ms,
    record_idempotent_resource, store_error, validate_communication_principal, validate_id,
    validate_idempotency_key, CommunicationPrincipal, CommunicationStoreError, ConversationAccess,
    CONVERSATION_ID_PREFIX, CONVERSATION_MESSAGE_ID_PREFIX, DURABLE_AGENT_ID_PREFIX,
};
use super::Database;
use rusqlite::{
    params, types::Type, Connection, OptionalExtension, Transaction, TransactionBehavior,
};
use serde::Serialize;
use serde_json::json;
use webcodex_core::coding_agent::{
    merge_coding_agent_run_snapshot, validate_coding_agent_run_snapshot, CodingAgentExecutionState,
    CodingAgentObservationMerge, CodingAgentRunSnapshot, CodingAgentRunState, CodingAgentTerminal,
};

pub(crate) const AGENT_TASK_ID_PREFIX: &str = "wc_agent_task_";
pub(crate) const AGENT_TASK_ATTEMPT_ID_PREFIX: &str = "wc_agent_task_attempt_";
pub(crate) const AGENT_TASK_ATTEMPT_FENCE_PREFIX: &str = "wc_agent_task_fence_";

pub(crate) const MAX_AGENT_TASK_TITLE_CHARS: usize = 200;
pub(crate) const MAX_AGENT_TASK_INSTRUCTION_BYTES: usize = 8_192;
pub const MAX_AGENT_TASK_TERMINAL_TEXT_BYTES: usize = 4_096;
pub(crate) const MAX_AGENT_TASK_PROJECT_REF_CHARS: usize = 256;
pub const MAX_AGENT_TASK_LIST_LIMIT: usize = 100;
pub(crate) const DEFAULT_AGENT_TASK_ATTEMPT_LEASE_MS: i64 = 60_000;

const OP_CREATE_AGENT_TASK: &str = "create_agent_task";
const OP_START_AGENT_TASK_ATTEMPT: &str = "start_agent_task_attempt";
const OP_COMPLETE_AGENT_TASK_ATTEMPT: &str = "complete_agent_task_attempt";

#[derive(Debug, Clone)]
pub struct NewAgentTask {
    pub title: String,
    pub instruction: String,
    pub assignee_agent_id: Option<String>,
    pub source_conversation_id: Option<String>,
    pub source_message_id: Option<String>,
    pub referenced_project_id: Option<String>,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskState {
    Ready,
    Active,
    Succeeded,
    Failed,
}

impl AgentTaskState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Active => "active",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    fn from_db(value: &str, index: usize) -> rusqlite::Result<Self> {
        match value {
            "ready" => Ok(Self::Ready),
            "active" => Ok(Self::Active),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                Type::Text,
                format!("unsupported AgentTask state: {other}").into(),
            )),
        }
    }

    const fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskAttemptState {
    Active,
    Expired,
    Succeeded,
    Failed,
}

impl AgentTaskAttemptState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Expired => "expired",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    fn from_db(value: &str, index: usize) -> rusqlite::Result<Self> {
        match value {
            "active" => Ok(Self::Active),
            "expired" => Ok(Self::Expired),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                Type::Text,
                format!("unsupported AgentTaskAttempt state: {other}").into(),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskCodingRunDispatchState {
    Prepared,
    NotStarted,
    OutcomeUnknown,
    Bound,
    Terminal,
}

impl AgentTaskCodingRunDispatchState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::NotStarted => "not_started",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::Bound => "bound",
            Self::Terminal => "terminal",
        }
    }

    fn from_db(value: &str, index: usize) -> rusqlite::Result<Self> {
        match value {
            "prepared" => Ok(Self::Prepared),
            "not_started" => Ok(Self::NotStarted),
            "outcome_unknown" => Ok(Self::OutcomeUnknown),
            "bound" => Ok(Self::Bound),
            "terminal" => Ok(Self::Terminal),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                Type::Text,
                format!("unsupported AgentTask CodingAgent dispatch state: {other}").into(),
            )),
        }
    }

    const fn blocks_replacement(self) -> bool {
        matches!(self, Self::OutcomeUnknown | Self::Bound)
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskExecutionStatus {
    NotStarted,
    Active,
    WaitingPermission,
    OutcomeUnknown,
    Terminal,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskExecutionRecoveryKind {
    None,
    Observe,
    Reconcile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTaskCodingRunBindingIntent {
    pub run_id: String,
    pub runtime_project_id: String,
    pub provider_id: String,
    pub provider_instance_id: String,
    pub authority_fingerprint: String,
    pub coding_agent_intent_fingerprint: String,
    pub binding_intent_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTaskCodingRunObservation {
    pub run_id: String,
    pub runtime_project_id: String,
    pub provider_id: String,
    pub provider_instance_id: String,
    pub authority_fingerprint: String,
    pub coding_agent_intent_fingerprint: String,
    pub run_state: CodingAgentRunState,
    pub execution_state: CodingAgentExecutionState,
    pub observation_revision: i64,
    pub terminal_stop_reason: Option<String>,
    pub terminal_error_code: Option<String>,
    pub terminal_message: Option<String>,
    pub completed_at_unix: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTaskCodingRunBindingRecord {
    pub task_id: String,
    pub attempt_id: String,
    pub run_id: String,
    pub runtime_project_id: String,
    pub provider_id: String,
    pub provider_instance_id: String,
    pub authority_fingerprint: String,
    pub coding_agent_intent_fingerprint: String,
    pub binding_intent_fingerprint: String,
    pub dispatch_state: AgentTaskCodingRunDispatchState,
    pub last_observed_run_state: Option<CodingAgentRunState>,
    pub last_observed_execution_state: Option<CodingAgentExecutionState>,
    pub last_observation_revision: Option<i64>,
    pub terminal_stop_reason: Option<String>,
    pub terminal_error_code: Option<String>,
    pub terminal_message: Option<String>,
    pub completed_at_unix: Option<i64>,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
    pub terminal_at_unix_ms: Option<i64>,
}

impl AgentTaskCodingRunBindingRecord {
    fn execution_status(&self) -> AgentTaskExecutionStatus {
        match self.dispatch_state {
            AgentTaskCodingRunDispatchState::Prepared
            | AgentTaskCodingRunDispatchState::NotStarted => AgentTaskExecutionStatus::NotStarted,
            AgentTaskCodingRunDispatchState::OutcomeUnknown => {
                AgentTaskExecutionStatus::OutcomeUnknown
            }
            AgentTaskCodingRunDispatchState::Terminal => AgentTaskExecutionStatus::Terminal,
            AgentTaskCodingRunDispatchState::Bound => match self.last_observed_run_state.as_ref() {
                Some(CodingAgentRunState::WaitingPermission) => {
                    AgentTaskExecutionStatus::WaitingPermission
                }
                Some(CodingAgentRunState::Lost) => AgentTaskExecutionStatus::OutcomeUnknown,
                Some(
                    CodingAgentRunState::Completed
                    | CodingAgentRunState::Failed
                    | CodingAgentRunState::Cancelled,
                ) => AgentTaskExecutionStatus::Terminal,
                _ => AgentTaskExecutionStatus::Active,
            },
        }
    }

    fn recovery_kind(&self) -> AgentTaskExecutionRecoveryKind {
        match self.dispatch_state {
            AgentTaskCodingRunDispatchState::Prepared
            | AgentTaskCodingRunDispatchState::NotStarted
            | AgentTaskCodingRunDispatchState::Terminal => AgentTaskExecutionRecoveryKind::None,
            AgentTaskCodingRunDispatchState::OutcomeUnknown => {
                AgentTaskExecutionRecoveryKind::Reconcile
            }
            AgentTaskCodingRunDispatchState::Bound => match self.execution_status() {
                AgentTaskExecutionStatus::Active | AgentTaskExecutionStatus::WaitingPermission => {
                    AgentTaskExecutionRecoveryKind::Observe
                }
                AgentTaskExecutionStatus::OutcomeUnknown | AgentTaskExecutionStatus::Terminal => {
                    AgentTaskExecutionRecoveryKind::Reconcile
                }
                AgentTaskExecutionStatus::NotStarted => AgentTaskExecutionRecoveryKind::None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTaskCodingRunStartContext {
    pub task: AgentTaskDetail,
    pub attempt: AgentTaskAttemptRecord,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTaskCodingRunPrepared {
    pub binding: AgentTaskCodingRunBindingRecord,
    pub replayed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTaskCodingRunDispatchClaim {
    pub binding: AgentTaskCodingRunBindingRecord,
    pub may_dispatch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTaskCodingRunReconcileMutation {
    pub task: AgentTaskSummary,
    pub attempt: AgentTaskAttemptRecord,
    pub binding: AgentTaskCodingRunBindingRecord,
    pub state_changed: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentTaskAttemptRecord {
    pub attempt_id: String,
    pub task_id: String,
    pub attempt_number: i64,
    pub assignee_agent_id: String,
    pub state: AgentTaskAttemptState,
    pub lease_expires_at_unix_ms: i64,
    pub lease_active: bool,
    pub attempt_controller_generation: i64,
    pub created_at_unix_ms: i64,
    pub started_at_unix_ms: i64,
    pub terminal_at_unix_ms: Option<i64>,
    pub terminal_result: Option<String>,
    pub terminal_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentTaskSummary {
    pub task_id: String,
    pub assignee_agent_id: Option<String>,
    pub title: String,
    pub source_conversation_id: Option<String>,
    pub source_message_id: Option<String>,
    pub referenced_project_id: Option<String>,
    pub state: AgentTaskState,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
    pub terminal_at_unix_ms: Option<i64>,
    pub latest_attempt: Option<AgentTaskAttemptRecord>,
    pub execution_bound: bool,
    pub execution_status: Option<AgentTaskExecutionStatus>,
    pub recovery_kind: AgentTaskExecutionRecoveryKind,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentTaskDetail {
    pub summary: AgentTaskSummary,
    pub instruction: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentTaskMutation {
    pub task: AgentTaskDetail,
    pub created: bool,
    pub replayed: bool,
    pub state_changed: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentTaskPage {
    pub total_count: i64,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub truncated: bool,
    pub tasks: Vec<AgentTaskSummary>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentTaskAttemptStartMutation {
    pub task: AgentTaskSummary,
    pub attempt: AgentTaskAttemptRecord,
    pub attempt_fence: String,
    pub replayed: bool,
    pub state_changed: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentTaskAttemptHeartbeatMutation {
    pub task: AgentTaskSummary,
    pub attempt: AgentTaskAttemptRecord,
    pub state_changed: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentTaskAttemptCompletionMutation {
    pub task: AgentTaskSummary,
    pub attempt: AgentTaskAttemptRecord,
    pub replayed: bool,
    pub state_changed: bool,
}

#[derive(Debug, Clone)]
struct StoredAttempt {
    attempt_id: String,
    task_id: String,
    attempt_number: i64,
    assignee_agent_id: String,
    stored_state: AgentTaskAttemptState,
    lease_expires_at_unix_ms: i64,
    attempt_fence: String,
    attempt_controller_generation: i64,
    created_at_unix_ms: i64,
    started_at_unix_ms: i64,
    terminal_at_unix_ms: Option<i64>,
    terminal_result: Option<String>,
    terminal_reason: Option<String>,
}

impl StoredAttempt {
    fn effective_state(&self, now: i64) -> AgentTaskAttemptState {
        if self.stored_state == AgentTaskAttemptState::Active
            && self.lease_expires_at_unix_ms <= now
        {
            AgentTaskAttemptState::Expired
        } else {
            self.stored_state
        }
    }

    fn record(&self, now: i64) -> AgentTaskAttemptRecord {
        let state = self.effective_state(now);
        AgentTaskAttemptRecord {
            attempt_id: self.attempt_id.clone(),
            task_id: self.task_id.clone(),
            attempt_number: self.attempt_number,
            assignee_agent_id: self.assignee_agent_id.clone(),
            state,
            lease_expires_at_unix_ms: self.lease_expires_at_unix_ms,
            lease_active: state == AgentTaskAttemptState::Active,
            attempt_controller_generation: self.attempt_controller_generation,
            created_at_unix_ms: self.created_at_unix_ms,
            started_at_unix_ms: self.started_at_unix_ms,
            terminal_at_unix_ms: self.terminal_at_unix_ms,
            terminal_result: self.terminal_result.clone(),
            terminal_reason: self.terminal_reason.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct StoredTask {
    task_id: String,
    assignee_agent_id: Option<String>,
    title: String,
    instruction: String,
    source_conversation_id: Option<String>,
    source_message_id: Option<String>,
    referenced_project_id: Option<String>,
    stored_state: AgentTaskState,
    created_at_unix_ms: i64,
    updated_at_unix_ms: i64,
    terminal_at_unix_ms: Option<i64>,
    latest_attempt: Option<StoredAttempt>,
    latest_coding_run: Option<AgentTaskCodingRunBindingRecord>,
}

impl StoredTask {
    fn effective_state(&self, now: i64) -> AgentTaskState {
        if self.stored_state == AgentTaskState::Active
            && self.latest_attempt.as_ref().is_some_and(|attempt| {
                attempt.effective_state(now) == AgentTaskAttemptState::Expired
            })
            && !self
                .latest_coding_run
                .as_ref()
                .is_some_and(|binding| binding.dispatch_state.blocks_replacement())
        {
            AgentTaskState::Ready
        } else {
            self.stored_state
        }
    }

    fn summary(&self, now: i64) -> AgentTaskSummary {
        AgentTaskSummary {
            task_id: self.task_id.clone(),
            assignee_agent_id: self.assignee_agent_id.clone(),
            title: self.title.clone(),
            source_conversation_id: self.source_conversation_id.clone(),
            source_message_id: self.source_message_id.clone(),
            referenced_project_id: self.referenced_project_id.clone(),
            state: self.effective_state(now),
            created_at_unix_ms: self.created_at_unix_ms,
            updated_at_unix_ms: self.updated_at_unix_ms,
            terminal_at_unix_ms: self.terminal_at_unix_ms,
            latest_attempt: self
                .latest_attempt
                .as_ref()
                .map(|attempt| attempt.record(now)),
            execution_bound: self.latest_coding_run.is_some(),
            execution_status: self
                .latest_coding_run
                .as_ref()
                .map(AgentTaskCodingRunBindingRecord::execution_status),
            recovery_kind: self
                .latest_coding_run
                .as_ref()
                .map(AgentTaskCodingRunBindingRecord::recovery_kind)
                .unwrap_or(AgentTaskExecutionRecoveryKind::None),
        }
    }

    fn detail(&self, now: i64) -> AgentTaskDetail {
        AgentTaskDetail {
            summary: self.summary(now),
            instruction: self.instruction.clone(),
        }
    }
}

impl Database {
    pub(crate) fn ensure_agent_task_schema(conn: &mut Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS wc_agent_tasks (
                task_id TEXT PRIMARY KEY,
                owner_principal_kind TEXT NOT NULL,
                owner_principal_digest TEXT NOT NULL,
                assignee_agent_id TEXT,
                title TEXT NOT NULL,
                instruction TEXT NOT NULL,
                source_conversation_id TEXT,
                source_message_id TEXT,
                referenced_project_id TEXT,
                state TEXT NOT NULL CHECK(state IN ('ready', 'active', 'succeeded', 'failed')),
                latest_attempt_id TEXT,
                terminal_attempt_id TEXT,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                terminal_at_unix_ms INTEGER,
                CHECK(
                    (state IN ('ready', 'active') AND terminal_attempt_id IS NULL AND terminal_at_unix_ms IS NULL)
                    OR (state IN ('succeeded', 'failed') AND terminal_attempt_id IS NOT NULL AND terminal_at_unix_ms IS NOT NULL)
                ),
                FOREIGN KEY(assignee_agent_id) REFERENCES wc_agent_identities(agent_id),
                FOREIGN KEY(source_conversation_id) REFERENCES wc_conversations(conversation_id),
                FOREIGN KEY(source_message_id) REFERENCES wc_conversation_messages(message_id)
            );
            CREATE INDEX IF NOT EXISTS idx_wc_agent_tasks_owner_updated
                ON wc_agent_tasks(owner_principal_digest, updated_at_unix_ms DESC, task_id);
            CREATE INDEX IF NOT EXISTS idx_wc_agent_tasks_assignee
                ON wc_agent_tasks(owner_principal_digest, assignee_agent_id, updated_at_unix_ms DESC);

            CREATE TABLE IF NOT EXISTS wc_agent_task_attempts (
                attempt_id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                attempt_number INTEGER NOT NULL CHECK(attempt_number >= 1),
                assignee_agent_id TEXT NOT NULL,
                state TEXT NOT NULL CHECK(state IN ('active', 'expired', 'succeeded', 'failed')),
                lease_expires_at_unix_ms INTEGER NOT NULL,
                attempt_fence TEXT NOT NULL UNIQUE,
                attempt_controller_generation INTEGER NOT NULL CHECK(attempt_controller_generation >= 1),
                created_at_unix_ms INTEGER NOT NULL,
                started_at_unix_ms INTEGER NOT NULL,
                terminal_at_unix_ms INTEGER,
                terminal_result TEXT,
                terminal_reason TEXT,
                CHECK(
                    (state = 'active' AND terminal_at_unix_ms IS NULL)
                    OR (state != 'active' AND terminal_at_unix_ms IS NOT NULL)
                ),
                UNIQUE(task_id, attempt_number),
                FOREIGN KEY(task_id) REFERENCES wc_agent_tasks(task_id),
                FOREIGN KEY(assignee_agent_id) REFERENCES wc_agent_identities(agent_id)
            );
            CREATE TABLE IF NOT EXISTS wc_agent_task_coding_runs (
                task_id TEXT NOT NULL,
                attempt_id TEXT NOT NULL UNIQUE,
                run_id TEXT NOT NULL UNIQUE,
                runtime_project_id TEXT NOT NULL,
                provider_id TEXT NOT NULL,
                provider_instance_id TEXT NOT NULL,
                authority_fingerprint TEXT NOT NULL,
                coding_agent_intent_fingerprint TEXT NOT NULL,
                binding_intent_fingerprint TEXT NOT NULL,
                dispatch_state TEXT NOT NULL CHECK(dispatch_state IN ('prepared', 'not_started', 'outcome_unknown', 'bound', 'terminal')),
                last_observed_run_state TEXT,
                last_observed_execution_state TEXT,
                last_observation_revision INTEGER,
                terminal_stop_reason TEXT,
                terminal_error_code TEXT,
                terminal_message TEXT,
                completed_at_unix INTEGER,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                terminal_at_unix_ms INTEGER,
                FOREIGN KEY(task_id) REFERENCES wc_agent_tasks(task_id),
                FOREIGN KEY(attempt_id) REFERENCES wc_agent_task_attempts(attempt_id)
            );
            CREATE INDEX IF NOT EXISTS idx_wc_agent_task_coding_runs_task
                ON wc_agent_task_coding_runs(task_id, updated_at_unix_ms DESC);

            CREATE UNIQUE INDEX IF NOT EXISTS idx_wc_agent_task_attempts_one_active
                ON wc_agent_task_attempts(task_id) WHERE state = 'active';
            CREATE INDEX IF NOT EXISTS idx_wc_agent_task_attempts_task_number
                ON wc_agent_task_attempts(task_id, attempt_number DESC);
            CREATE INDEX IF NOT EXISTS idx_wc_agent_task_attempts_assignee
                ON wc_agent_task_attempts(assignee_agent_id, state, lease_expires_at_unix_ms);
            ",
        )?;
        Ok(())
    }

    pub fn create_agent_task(
        &self,
        principal: &CommunicationPrincipal,
        input: NewAgentTask,
    ) -> Result<AgentTaskMutation, CommunicationStoreError> {
        self.create_agent_task_at(principal, input, now_unix_ms())
    }

    pub(crate) fn create_agent_task_at(
        &self,
        principal: &CommunicationPrincipal,
        input: NewAgentTask,
        now: i64,
    ) -> Result<AgentTaskMutation, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        let title = validate_title(&input.title)?;
        let instruction = validate_instruction(&input.instruction)?;
        let assignee_agent_id = validate_optional_id(
            input.assignee_agent_id.as_deref(),
            DURABLE_AGENT_ID_PREFIX,
            "invalid_agent_id",
        )?;
        let source_conversation_id = validate_optional_id(
            input.source_conversation_id.as_deref(),
            CONVERSATION_ID_PREFIX,
            "invalid_conversation_id",
        )?;
        let source_message_id = validate_optional_id(
            input.source_message_id.as_deref(),
            CONVERSATION_MESSAGE_ID_PREFIX,
            "invalid_message_id",
        )?;
        if source_message_id.is_some() && source_conversation_id.is_none() {
            return Err(CommunicationStoreError::new(
                "agent_task_source_invalid",
                "source_message_id requires source_conversation_id",
            ));
        }
        let referenced_project_id =
            validate_project_reference(input.referenced_project_id.as_deref())?;
        let idempotency_key = validate_idempotency_key(&input.idempotency_key)?;
        let request_hash = task_request_hash(&json!({
            "title": title,
            "instruction": instruction,
            "assignee_agent_id": assignee_agent_id,
            "source_conversation_id": source_conversation_id,
            "source_message_id": source_message_id,
            "referenced_project_id": referenced_project_id,
        }));

        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;

        if let Some(task_id) = lookup_idempotent_resource(
            &transaction,
            principal,
            OP_CREATE_AGENT_TASK,
            &idempotency_key,
            &request_hash,
        )? {
            let task = load_owned_task(&transaction, principal, &task_id, now)?;
            transaction.commit().map_err(store_error)?;
            return Ok(AgentTaskMutation {
                task: task.detail(now),
                created: false,
                replayed: true,
                state_changed: false,
            });
        }

        if let Some(agent_id) = assignee_agent_id.as_deref() {
            require_owned_agent(&transaction, principal, agent_id)?;
        }
        validate_source_references(
            &transaction,
            principal,
            source_conversation_id.as_deref(),
            source_message_id.as_deref(),
        )?;

        let task_id = new_id(AGENT_TASK_ID_PREFIX);
        transaction
            .execute(
                "INSERT INTO wc_agent_tasks (
                    task_id, owner_principal_kind, owner_principal_digest,
                    assignee_agent_id, title, instruction, source_conversation_id,
                    source_message_id, referenced_project_id, state, latest_attempt_id,
                    terminal_attempt_id, created_at_unix_ms, updated_at_unix_ms,
                    terminal_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'ready', NULL, NULL, ?10, ?10, NULL)",
                params![
                    task_id,
                    principal.kind,
                    principal.digest,
                    assignee_agent_id,
                    title,
                    instruction,
                    source_conversation_id,
                    source_message_id,
                    referenced_project_id,
                    now,
                ],
            )
            .map_err(store_error)?;
        record_idempotent_resource(
            &transaction,
            principal,
            OP_CREATE_AGENT_TASK,
            &idempotency_key,
            &request_hash,
            &task_id,
            now,
        )?;
        let task = load_owned_task(&transaction, principal, &task_id, now)?;
        transaction.commit().map_err(store_error)?;
        Ok(AgentTaskMutation {
            task: task.detail(now),
            created: true,
            replayed: false,
            state_changed: true,
        })
    }

    pub fn list_agent_tasks(
        &self,
        principal: &CommunicationPrincipal,
        assignee_agent_id: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<AgentTaskPage, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        let assignee_agent_id = validate_optional_id(
            assignee_agent_id,
            DURABLE_AGENT_ID_PREFIX,
            "invalid_agent_id",
        )?;
        if limit == 0 || limit > MAX_AGENT_TASK_LIST_LIMIT {
            return Err(CommunicationStoreError::new(
                "invalid_agent_task_list_limit",
                format!("limit must be 1..={MAX_AGENT_TASK_LIST_LIMIT}"),
            ));
        }
        let now = now_unix_ms();
        let conn = self.conn.lock().unwrap();
        let (total_count, task_ids) = if let Some(agent_id) = assignee_agent_id.as_deref() {
            let total_count = conn
                .query_row(
                    "SELECT COUNT(*) FROM wc_agent_tasks
                     WHERE owner_principal_kind = ?1 AND owner_principal_digest = ?2
                       AND assignee_agent_id = ?3",
                    params![principal.kind, principal.digest, agent_id],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(store_error)?;
            let mut statement = conn
                .prepare(
                    "SELECT task_id FROM wc_agent_tasks
                     WHERE owner_principal_kind = ?1 AND owner_principal_digest = ?2
                       AND assignee_agent_id = ?3
                     ORDER BY updated_at_unix_ms DESC, task_id
                     LIMIT ?4 OFFSET ?5",
                )
                .map_err(store_error)?;
            let ids = statement
                .query_map(
                    params![
                        principal.kind,
                        principal.digest,
                        agent_id,
                        limit as i64,
                        offset as i64
                    ],
                    |row| row.get::<_, String>(0),
                )
                .map_err(store_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(store_error)?;
            (total_count, ids)
        } else {
            let total_count = conn
                .query_row(
                    "SELECT COUNT(*) FROM wc_agent_tasks
                     WHERE owner_principal_kind = ?1 AND owner_principal_digest = ?2",
                    params![principal.kind, principal.digest],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(store_error)?;
            let mut statement = conn
                .prepare(
                    "SELECT task_id FROM wc_agent_tasks
                     WHERE owner_principal_kind = ?1 AND owner_principal_digest = ?2
                     ORDER BY updated_at_unix_ms DESC, task_id
                     LIMIT ?3 OFFSET ?4",
                )
                .map_err(store_error)?;
            let ids = statement
                .query_map(
                    params![
                        principal.kind,
                        principal.digest,
                        limit as i64,
                        offset as i64
                    ],
                    |row| row.get::<_, String>(0),
                )
                .map_err(store_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(store_error)?;
            (total_count, ids)
        };
        let tasks = task_ids
            .iter()
            .map(|task_id| {
                load_owned_task(&conn, principal, task_id, now).map(|task| task.summary(now))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let next_offset = if offset.saturating_add(tasks.len()) < total_count as usize {
            Some(offset.saturating_add(tasks.len()))
        } else {
            None
        };
        Ok(AgentTaskPage {
            total_count,
            offset,
            next_offset,
            truncated: next_offset.is_some(),
            tasks,
        })
    }

    pub fn read_agent_task(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
    ) -> Result<AgentTaskDetail, CommunicationStoreError> {
        self.read_agent_task_at(principal, task_id, now_unix_ms())
    }

    pub(crate) fn read_agent_task_at(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        now: i64,
    ) -> Result<AgentTaskDetail, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        validate_id(task_id, AGENT_TASK_ID_PREFIX, "invalid_agent_task_id")?;
        let conn = self.conn.lock().unwrap();
        Ok(load_owned_task(&conn, principal, task_id, now)?.detail(now))
    }

    pub fn assign_agent_task(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        assignee_agent_id: &str,
    ) -> Result<AgentTaskMutation, CommunicationStoreError> {
        self.assign_agent_task_with_now(principal, task_id, assignee_agent_id, None)
    }

    #[cfg(test)]
    pub(crate) fn assign_agent_task_at(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        assignee_agent_id: &str,
        now: i64,
    ) -> Result<AgentTaskMutation, CommunicationStoreError> {
        self.assign_agent_task_with_now(principal, task_id, assignee_agent_id, Some(now))
    }

    fn assign_agent_task_with_now(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        assignee_agent_id: &str,
        now: Option<i64>,
    ) -> Result<AgentTaskMutation, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        validate_id(task_id, AGENT_TASK_ID_PREFIX, "invalid_agent_task_id")?;
        validate_id(
            assignee_agent_id,
            DURABLE_AGENT_ID_PREFIX,
            "invalid_agent_id",
        )?;
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let now = now.unwrap_or_else(now_unix_ms);
        let mut task = load_owned_task(&transaction, principal, task_id, now)?;
        if task.stored_state.terminal() {
            return Err(task_terminal_error());
        }
        require_owned_agent(&transaction, principal, assignee_agent_id)?;
        materialize_expired_latest_attempt(&transaction, &mut task, now)?;
        if task.assignee_agent_id.as_deref() != Some(assignee_agent_id) {
            if let Some(error) = blocking_execution_error(task.latest_coding_run.as_ref()) {
                return Err(error);
            }
        }
        if task
            .latest_attempt
            .as_ref()
            .is_some_and(|attempt| attempt.stored_state == AgentTaskAttemptState::Active)
        {
            if task.assignee_agent_id.as_deref() == Some(assignee_agent_id) {
                transaction.commit().map_err(store_error)?;
                return Ok(AgentTaskMutation {
                    task: task.detail(now),
                    created: false,
                    replayed: false,
                    state_changed: false,
                });
            }
            return Err(CommunicationStoreError::new(
                "agent_task_attempt_active",
                "AgentTask has an unexpired active Attempt; reassignment is fenced",
            ));
        }
        if task.assignee_agent_id.as_deref() == Some(assignee_agent_id) {
            transaction.commit().map_err(store_error)?;
            return Ok(AgentTaskMutation {
                task: task.detail(now),
                created: false,
                replayed: false,
                state_changed: false,
            });
        }
        transaction
            .execute(
                "UPDATE wc_agent_tasks
                 SET assignee_agent_id = ?2, state = 'ready', updated_at_unix_ms = ?3
                 WHERE task_id = ?1",
                params![task_id, assignee_agent_id, now.max(task.updated_at_unix_ms)],
            )
            .map_err(store_error)?;
        task = load_owned_task(&transaction, principal, task_id, now)?;
        transaction.commit().map_err(store_error)?;
        Ok(AgentTaskMutation {
            task: task.detail(now),
            created: false,
            replayed: false,
            state_changed: true,
        })
    }

    pub fn start_agent_task_attempt(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        assignee_agent_id: &str,
        idempotency_key: &str,
    ) -> Result<AgentTaskAttemptStartMutation, CommunicationStoreError> {
        self.start_agent_task_attempt_with_now(
            principal,
            task_id,
            assignee_agent_id,
            idempotency_key,
            None,
        )
    }

    #[cfg(test)]
    pub(crate) fn start_agent_task_attempt_at(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        assignee_agent_id: &str,
        idempotency_key: &str,
        now: i64,
    ) -> Result<AgentTaskAttemptStartMutation, CommunicationStoreError> {
        self.start_agent_task_attempt_with_now(
            principal,
            task_id,
            assignee_agent_id,
            idempotency_key,
            Some(now),
        )
    }

    fn start_agent_task_attempt_with_now(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        assignee_agent_id: &str,
        idempotency_key: &str,
        now: Option<i64>,
    ) -> Result<AgentTaskAttemptStartMutation, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        validate_id(task_id, AGENT_TASK_ID_PREFIX, "invalid_agent_task_id")?;
        validate_id(
            assignee_agent_id,
            DURABLE_AGENT_ID_PREFIX,
            "invalid_agent_id",
        )?;
        let idempotency_key = validate_idempotency_key(idempotency_key)?;
        let request_hash = task_request_hash(&json!({
            "task_id": task_id,
            "assignee_agent_id": assignee_agent_id,
        }));
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let now = now.unwrap_or_else(now_unix_ms);
        let mut task = load_owned_task(&transaction, principal, task_id, now)?;

        if let Some(attempt_id) = lookup_idempotent_resource(
            &transaction,
            principal,
            OP_START_AGENT_TASK_ATTEMPT,
            &idempotency_key,
            &request_hash,
        )? {
            let attempt = load_attempt_for_task(&transaction, task_id, &attempt_id, now)?
                .ok_or_else(|| {
                    CommunicationStoreError::new(
                        "agent_task_attempt_not_found",
                        "AgentTaskAttempt does not exist",
                    )
                })?;
            let record = attempt.record(now);
            let fence = attempt.attempt_fence.clone();
            transaction.commit().map_err(store_error)?;
            return Ok(AgentTaskAttemptStartMutation {
                task: task.summary(now),
                attempt: record,
                attempt_fence: fence,
                replayed: true,
                state_changed: false,
            });
        }

        if task.stored_state.terminal() {
            return Err(task_terminal_error());
        }
        let current_assignee = task.assignee_agent_id.as_deref().ok_or_else(|| {
            CommunicationStoreError::new(
                "agent_task_unassigned",
                "AgentTask must have an explicit assignee before an Attempt can start",
            )
        })?;
        if current_assignee != assignee_agent_id {
            return Err(CommunicationStoreError::new(
                "agent_task_assignee_mismatch",
                "Requested Agent does not match the AgentTask current assignee",
            ));
        }
        require_owned_agent(&transaction, principal, assignee_agent_id)?;
        materialize_expired_latest_attempt(&transaction, &mut task, now)?;
        if let Some(error) = blocking_execution_error(task.latest_coding_run.as_ref()) {
            return Err(error);
        }
        if task
            .latest_attempt
            .as_ref()
            .is_some_and(|attempt| attempt.stored_state == AgentTaskAttemptState::Active)
        {
            return Err(CommunicationStoreError::new(
                "agent_task_attempt_active",
                "AgentTask already has an unexpired authoritative Attempt",
            ));
        }

        let attempt_number = task
            .latest_attempt
            .as_ref()
            .map(|attempt| attempt.attempt_number.saturating_add(1))
            .unwrap_or(1);
        let attempt_id = new_id(AGENT_TASK_ATTEMPT_ID_PREFIX);
        let attempt_fence = new_id(AGENT_TASK_ATTEMPT_FENCE_PREFIX);
        let lease_expires_at = now.saturating_add(DEFAULT_AGENT_TASK_ATTEMPT_LEASE_MS);
        transaction
            .execute(
                "INSERT INTO wc_agent_task_attempts (
                    attempt_id, task_id, attempt_number, assignee_agent_id, state,
                    lease_expires_at_unix_ms, attempt_fence, attempt_controller_generation,
                    created_at_unix_ms, started_at_unix_ms, terminal_at_unix_ms,
                    terminal_result, terminal_reason
                 ) VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, 1, ?7, ?7, NULL, NULL, NULL)",
                params![
                    attempt_id,
                    task_id,
                    attempt_number,
                    assignee_agent_id,
                    lease_expires_at,
                    attempt_fence,
                    now,
                ],
            )
            .map_err(store_error)?;
        transaction
            .execute(
                "UPDATE wc_agent_tasks
                 SET state = 'active', latest_attempt_id = ?2, updated_at_unix_ms = ?3
                 WHERE task_id = ?1",
                params![task_id, attempt_id, now.max(task.updated_at_unix_ms)],
            )
            .map_err(store_error)?;
        record_idempotent_resource(
            &transaction,
            principal,
            OP_START_AGENT_TASK_ATTEMPT,
            &idempotency_key,
            &request_hash,
            &attempt_id,
            now,
        )?;
        task = load_owned_task(&transaction, principal, task_id, now)?;
        let attempt =
            load_attempt_for_task(&transaction, task_id, &attempt_id, now)?.ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_attempt_not_found",
                    "AgentTaskAttempt disappeared after creation",
                )
            })?;
        let record = attempt.record(now);
        let fence = attempt.attempt_fence.clone();
        transaction.commit().map_err(store_error)?;
        Ok(AgentTaskAttemptStartMutation {
            task: task.summary(now),
            attempt: record,
            attempt_fence: fence,
            replayed: false,
            state_changed: true,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn heartbeat_agent_task_attempt(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        attempt_id: &str,
        assignee_agent_id: &str,
        attempt_fence: &str,
        attempt_controller_generation: i64,
    ) -> Result<AgentTaskAttemptHeartbeatMutation, CommunicationStoreError> {
        self.heartbeat_agent_task_attempt_with_now(
            principal,
            task_id,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
            None,
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn heartbeat_agent_task_attempt_at(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        attempt_id: &str,
        assignee_agent_id: &str,
        attempt_fence: &str,
        attempt_controller_generation: i64,
        now: i64,
    ) -> Result<AgentTaskAttemptHeartbeatMutation, CommunicationStoreError> {
        self.heartbeat_agent_task_attempt_with_now(
            principal,
            task_id,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
            Some(now),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn heartbeat_agent_task_attempt_with_now(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        attempt_id: &str,
        assignee_agent_id: &str,
        attempt_fence: &str,
        attempt_controller_generation: i64,
        now: Option<i64>,
    ) -> Result<AgentTaskAttemptHeartbeatMutation, CommunicationStoreError> {
        validate_attempt_mutation_inputs(
            principal,
            task_id,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
        )?;
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let now = now.unwrap_or_else(now_unix_ms);
        let task = load_owned_task(&transaction, principal, task_id, now)?;
        let attempt = require_current_attempt(
            &transaction,
            &task,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
            now,
        )?;
        let new_lease_expires_at = attempt
            .lease_expires_at_unix_ms
            .max(now.saturating_add(DEFAULT_AGENT_TASK_ATTEMPT_LEASE_MS));
        transaction
            .execute(
                "UPDATE wc_agent_task_attempts
                 SET lease_expires_at_unix_ms = ?2
                 WHERE attempt_id = ?1",
                params![attempt_id, new_lease_expires_at],
            )
            .map_err(store_error)?;
        transaction
            .execute(
                "UPDATE wc_agent_tasks SET updated_at_unix_ms = MAX(updated_at_unix_ms, ?2)
                 WHERE task_id = ?1",
                params![task_id, now],
            )
            .map_err(store_error)?;
        let task = load_owned_task(&transaction, principal, task_id, now)?;
        let attempt =
            load_attempt_for_task(&transaction, task_id, attempt_id, now)?.ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_attempt_not_found",
                    "AgentTaskAttempt disappeared after heartbeat",
                )
            })?;
        transaction.commit().map_err(store_error)?;
        Ok(AgentTaskAttemptHeartbeatMutation {
            task: task.summary(now),
            attempt: attempt.record(now),
            state_changed: true,
        })
    }

    /// Replace only the carrier/controller generation for the same live Attempt.
    /// A3 keeps this internal until a concrete backend needs it; the durable
    /// primitive exists now so stale carriers are fenced independently from
    /// Attempt retry identity.
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn replace_agent_task_attempt_controller_at(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        attempt_id: &str,
        assignee_agent_id: &str,
        attempt_fence: &str,
        expected_controller_generation: i64,
        now: i64,
    ) -> Result<AgentTaskAttemptRecord, CommunicationStoreError> {
        validate_attempt_mutation_inputs(
            principal,
            task_id,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            expected_controller_generation,
        )?;
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let task = load_owned_task(&transaction, principal, task_id, now)?;
        let attempt = require_current_attempt(
            &transaction,
            &task,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            expected_controller_generation,
            now,
        )?;
        let next_generation = attempt.attempt_controller_generation.saturating_add(1);
        transaction
            .execute(
                "UPDATE wc_agent_task_attempts
                 SET attempt_controller_generation = ?2
                 WHERE attempt_id = ?1",
                params![attempt_id, next_generation],
            )
            .map_err(store_error)?;
        transaction
            .execute(
                "UPDATE wc_agent_tasks SET updated_at_unix_ms = MAX(updated_at_unix_ms, ?2)
                 WHERE task_id = ?1",
                params![task_id, now],
            )
            .map_err(store_error)?;
        let attempt =
            load_attempt_for_task(&transaction, task_id, attempt_id, now)?.ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_attempt_not_found",
                    "AgentTaskAttempt disappeared after controller replacement",
                )
            })?;
        let record = attempt.record(now);
        transaction.commit().map_err(store_error)?;
        Ok(record)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn complete_agent_task_attempt(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        attempt_id: &str,
        assignee_agent_id: &str,
        attempt_fence: &str,
        attempt_controller_generation: i64,
        outcome: AgentTaskState,
        terminal_result: Option<&str>,
        terminal_reason: Option<&str>,
        completion_key: &str,
    ) -> Result<AgentTaskAttemptCompletionMutation, CommunicationStoreError> {
        self.complete_agent_task_attempt_with_now(
            principal,
            task_id,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
            outcome,
            terminal_result,
            terminal_reason,
            completion_key,
            None,
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn complete_agent_task_attempt_at(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        attempt_id: &str,
        assignee_agent_id: &str,
        attempt_fence: &str,
        attempt_controller_generation: i64,
        outcome: AgentTaskState,
        terminal_result: Option<&str>,
        terminal_reason: Option<&str>,
        completion_key: &str,
        now: i64,
    ) -> Result<AgentTaskAttemptCompletionMutation, CommunicationStoreError> {
        self.complete_agent_task_attempt_with_now(
            principal,
            task_id,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
            outcome,
            terminal_result,
            terminal_reason,
            completion_key,
            Some(now),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn complete_agent_task_attempt_with_now(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        attempt_id: &str,
        assignee_agent_id: &str,
        attempt_fence: &str,
        attempt_controller_generation: i64,
        outcome: AgentTaskState,
        terminal_result: Option<&str>,
        terminal_reason: Option<&str>,
        completion_key: &str,
        now: Option<i64>,
    ) -> Result<AgentTaskAttemptCompletionMutation, CommunicationStoreError> {
        validate_attempt_mutation_inputs(
            principal,
            task_id,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
        )?;
        if !outcome.terminal() {
            return Err(CommunicationStoreError::new(
                "invalid_agent_task_completion_outcome",
                "AgentTaskAttempt completion outcome must be succeeded or failed",
            ));
        }
        let terminal_result = validate_optional_terminal_text(terminal_result, "terminal_result")?;
        let terminal_reason = validate_optional_terminal_text(terminal_reason, "terminal_reason")?;
        let completion_key = validate_idempotency_key(completion_key)?;
        let request_hash = task_request_hash(&json!({
            "task_id": task_id,
            "attempt_id": attempt_id,
            "assignee_agent_id": assignee_agent_id,
            "attempt_fence": attempt_fence,
            "attempt_controller_generation": attempt_controller_generation,
            "outcome": outcome.as_str(),
            "terminal_result": terminal_result,
            "terminal_reason": terminal_reason,
        }));
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let now = now.unwrap_or_else(now_unix_ms);
        let task = load_owned_task(&transaction, principal, task_id, now)?;

        if let Some(replayed_attempt_id) = lookup_idempotent_resource(
            &transaction,
            principal,
            OP_COMPLETE_AGENT_TASK_ATTEMPT,
            &completion_key,
            &request_hash,
        )? {
            if replayed_attempt_id != attempt_id {
                return Err(CommunicationStoreError::new(
                    "agent_task_idempotency_invariant",
                    "Completion replay points to a different AgentTaskAttempt",
                ));
            }
            let attempt = load_attempt_for_task(&transaction, task_id, attempt_id, now)?
                .ok_or_else(|| {
                    CommunicationStoreError::new(
                        "agent_task_attempt_not_found",
                        "AgentTaskAttempt does not exist",
                    )
                })?;
            transaction.commit().map_err(store_error)?;
            return Ok(AgentTaskAttemptCompletionMutation {
                task: task.summary(now),
                attempt: attempt.record(now),
                replayed: true,
                state_changed: false,
            });
        }

        let _attempt = require_current_attempt(
            &transaction,
            &task,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
            now,
        )?;
        let attempt_state = match outcome {
            AgentTaskState::Succeeded => AgentTaskAttemptState::Succeeded,
            AgentTaskState::Failed => AgentTaskAttemptState::Failed,
            AgentTaskState::Ready | AgentTaskState::Active => {
                unreachable!("validated terminal outcome")
            }
        };
        transaction
            .execute(
                "UPDATE wc_agent_task_attempts
                 SET state = ?2, terminal_at_unix_ms = ?3,
                     terminal_result = ?4, terminal_reason = ?5
                 WHERE attempt_id = ?1",
                params![
                    attempt_id,
                    attempt_state.as_str(),
                    now,
                    terminal_result,
                    terminal_reason,
                ],
            )
            .map_err(store_error)?;
        transaction
            .execute(
                "UPDATE wc_agent_tasks
                 SET state = ?2, terminal_attempt_id = ?3, terminal_at_unix_ms = ?4,
                     updated_at_unix_ms = MAX(updated_at_unix_ms, ?4)
                 WHERE task_id = ?1",
                params![task_id, outcome.as_str(), attempt_id, now],
            )
            .map_err(store_error)?;
        record_idempotent_resource(
            &transaction,
            principal,
            OP_COMPLETE_AGENT_TASK_ATTEMPT,
            &completion_key,
            &request_hash,
            attempt_id,
            now,
        )?;
        let task = load_owned_task(&transaction, principal, task_id, now)?;
        let attempt =
            load_attempt_for_task(&transaction, task_id, attempt_id, now)?.ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_attempt_not_found",
                    "AgentTaskAttempt disappeared after completion",
                )
            })?;
        transaction.commit().map_err(store_error)?;
        Ok(AgentTaskAttemptCompletionMutation {
            task: task.summary(now),
            attempt: attempt.record(now),
            replayed: false,
            state_changed: true,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn agent_task_coding_run_start_context(
        &self,
        principal: &CommunicationPrincipal,
        project: &str,
        task_id: &str,
        attempt_id: &str,
        assignee_agent_id: &str,
        attempt_fence: &str,
        attempt_controller_generation: i64,
    ) -> Result<AgentTaskCodingRunStartContext, CommunicationStoreError> {
        self.agent_task_coding_run_start_context_with_now(
            principal,
            project,
            task_id,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
            now_unix_ms(),
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn agent_task_coding_run_start_context_at(
        &self,
        principal: &CommunicationPrincipal,
        project: &str,
        task_id: &str,
        attempt_id: &str,
        assignee_agent_id: &str,
        attempt_fence: &str,
        attempt_controller_generation: i64,
        now: i64,
    ) -> Result<AgentTaskCodingRunStartContext, CommunicationStoreError> {
        self.agent_task_coding_run_start_context_with_now(
            principal,
            project,
            task_id,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
            now,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn agent_task_coding_run_start_context_with_now(
        &self,
        principal: &CommunicationPrincipal,
        project: &str,
        task_id: &str,
        attempt_id: &str,
        assignee_agent_id: &str,
        attempt_fence: &str,
        attempt_controller_generation: i64,
        now: i64,
    ) -> Result<AgentTaskCodingRunStartContext, CommunicationStoreError> {
        validate_attempt_mutation_inputs(
            principal,
            task_id,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
        )?;
        let conn = self.conn.lock().unwrap();
        let task = load_owned_task(&conn, principal, task_id, now)?;
        let referenced_project = task.referenced_project_id.as_deref().ok_or_else(|| {
            CommunicationStoreError::new(
                "agent_task_project_required",
                "AgentTask has no referenced_project_id and cannot dispatch a CodingAgentRun",
            )
        })?;
        if project != referenced_project {
            return Err(CommunicationStoreError::new(
                "agent_task_project_mismatch",
                "Requested Project does not match AgentTask referenced_project_id",
            ));
        }
        let attempt = require_current_attempt(
            &conn,
            &task,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
            now,
        )?;
        Ok(AgentTaskCodingRunStartContext {
            task: task.detail(now),
            attempt: attempt.record(now),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prepare_agent_task_coding_run(
        &self,
        principal: &CommunicationPrincipal,
        project: &str,
        task_id: &str,
        attempt_id: &str,
        assignee_agent_id: &str,
        attempt_fence: &str,
        attempt_controller_generation: i64,
        intent: &AgentTaskCodingRunBindingIntent,
    ) -> Result<AgentTaskCodingRunPrepared, CommunicationStoreError> {
        self.prepare_agent_task_coding_run_with_now(
            principal,
            project,
            task_id,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
            intent,
            None,
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_agent_task_coding_run_at(
        &self,
        principal: &CommunicationPrincipal,
        project: &str,
        task_id: &str,
        attempt_id: &str,
        assignee_agent_id: &str,
        attempt_fence: &str,
        attempt_controller_generation: i64,
        intent: &AgentTaskCodingRunBindingIntent,
        now: i64,
    ) -> Result<AgentTaskCodingRunPrepared, CommunicationStoreError> {
        self.prepare_agent_task_coding_run_with_now(
            principal,
            project,
            task_id,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
            intent,
            Some(now),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_agent_task_coding_run_with_now(
        &self,
        principal: &CommunicationPrincipal,
        project: &str,
        task_id: &str,
        attempt_id: &str,
        assignee_agent_id: &str,
        attempt_fence: &str,
        attempt_controller_generation: i64,
        intent: &AgentTaskCodingRunBindingIntent,
        now: Option<i64>,
    ) -> Result<AgentTaskCodingRunPrepared, CommunicationStoreError> {
        validate_attempt_mutation_inputs(
            principal,
            task_id,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
        )?;
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let now = now.unwrap_or_else(now_unix_ms);
        let task = load_owned_task(&transaction, principal, task_id, now)?;
        let referenced_project = task.referenced_project_id.as_deref().ok_or_else(|| {
            CommunicationStoreError::new(
                "agent_task_project_required",
                "AgentTask has no referenced_project_id and cannot dispatch a CodingAgentRun",
            )
        })?;
        if project != referenced_project || intent.runtime_project_id != referenced_project {
            return Err(CommunicationStoreError::new(
                "agent_task_project_mismatch",
                "CodingAgentRun Project does not match AgentTask referenced_project_id",
            ));
        }
        let _attempt = require_current_attempt(
            &transaction,
            &task,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
            now,
        )?;
        if let Some(mut existing) =
            load_coding_run_binding_for_attempt(&transaction, task_id, attempt_id)?
        {
            let same_intent = existing.run_id == intent.run_id
                && existing.runtime_project_id == intent.runtime_project_id
                && existing.provider_id == intent.provider_id
                && existing.authority_fingerprint == intent.authority_fingerprint
                && existing.coding_agent_intent_fingerprint
                    == intent.coding_agent_intent_fingerprint
                && existing.binding_intent_fingerprint == intent.binding_intent_fingerprint;
            if !same_intent {
                return Err(CommunicationStoreError::new(
                    "agent_task_coding_run_binding_conflict",
                    "AgentTaskAttempt is already bound to a different CodingAgentRun intent",
                ));
            }
            if existing.provider_instance_id != intent.provider_instance_id {
                if matches!(
                    existing.dispatch_state,
                    AgentTaskCodingRunDispatchState::Prepared
                        | AgentTaskCodingRunDispatchState::NotStarted
                ) {
                    transaction
                        .execute(
                            "UPDATE wc_agent_task_coding_runs
                             SET provider_instance_id = ?3, updated_at_unix_ms = MAX(updated_at_unix_ms, ?4)
                             WHERE task_id = ?1 AND attempt_id = ?2",
                            params![task_id, attempt_id, intent.provider_instance_id, now],
                        )
                        .map_err(store_error)?;
                    existing =
                        load_coding_run_binding_for_attempt(&transaction, task_id, attempt_id)?
                            .ok_or_else(|| {
                                CommunicationStoreError::new(
                                    "agent_task_storage_invariant",
                                    "AgentTask CodingAgent binding disappeared during replay",
                                )
                            })?;
                } else {
                    return Err(CommunicationStoreError::new(
                        "agent_task_coding_run_binding_conflict",
                        "AgentTaskAttempt CodingAgent provider instance cannot change after dispatch may have started",
                    ));
                }
            }
            transaction.commit().map_err(store_error)?;
            return Ok(AgentTaskCodingRunPrepared {
                binding: existing,
                replayed: true,
            });
        }
        let conflicting_attempt = transaction
            .query_row(
                "SELECT attempt_id FROM wc_agent_task_coding_runs WHERE run_id = ?1",
                [intent.run_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(store_error)?;
        if conflicting_attempt.is_some() {
            return Err(CommunicationStoreError::new(
                "agent_task_coding_run_binding_conflict",
                "CodingAgentRun is already bound to another AgentTaskAttempt",
            ));
        }
        transaction
            .execute(
                "INSERT INTO wc_agent_task_coding_runs (
                    task_id, attempt_id, run_id, runtime_project_id, provider_id,
                    provider_instance_id, authority_fingerprint, coding_agent_intent_fingerprint,
                    binding_intent_fingerprint, dispatch_state, created_at_unix_ms,
                    updated_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'prepared', ?10, ?10)",
                params![
                    task_id,
                    attempt_id,
                    intent.run_id,
                    intent.runtime_project_id,
                    intent.provider_id,
                    intent.provider_instance_id,
                    intent.authority_fingerprint,
                    intent.coding_agent_intent_fingerprint,
                    intent.binding_intent_fingerprint,
                    now,
                ],
            )
            .map_err(store_error)?;
        let binding = load_coding_run_binding_for_attempt(&transaction, task_id, attempt_id)?
            .ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_storage_invariant",
                    "AgentTask CodingAgent binding disappeared after prepare",
                )
            })?;
        transaction.commit().map_err(store_error)?;
        Ok(AgentTaskCodingRunPrepared {
            binding,
            replayed: false,
        })
    }

    pub fn read_agent_task_coding_run_binding(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        attempt_id: &str,
    ) -> Result<AgentTaskCodingRunBindingRecord, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        validate_id(task_id, AGENT_TASK_ID_PREFIX, "invalid_agent_task_id")?;
        validate_id(
            attempt_id,
            AGENT_TASK_ATTEMPT_ID_PREFIX,
            "invalid_agent_task_attempt_id",
        )?;
        let conn = self.conn.lock().unwrap();
        let task = load_owned_task(&conn, principal, task_id, now_unix_ms())?;
        load_attempt_for_task(&conn, task_id, attempt_id, now_unix_ms())?.ok_or_else(|| {
            CommunicationStoreError::new(
                "agent_task_attempt_not_found",
                "AgentTaskAttempt does not exist",
            )
        })?;
        load_coding_run_binding_for_attempt(&conn, &task.task_id, attempt_id)?.ok_or_else(|| {
            CommunicationStoreError::new(
                "agent_task_coding_run_not_found",
                "AgentTaskAttempt has no bound CodingAgentRun",
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn claim_agent_task_coding_run_dispatch(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        attempt_id: &str,
        assignee_agent_id: &str,
        attempt_fence: &str,
        attempt_controller_generation: i64,
        binding_intent_fingerprint: &str,
    ) -> Result<AgentTaskCodingRunDispatchClaim, CommunicationStoreError> {
        validate_attempt_mutation_inputs(
            principal,
            task_id,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
        )?;
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let now = now_unix_ms();
        let task = load_owned_task(&transaction, principal, task_id, now)?;
        let _attempt = require_current_attempt(
            &transaction,
            &task,
            attempt_id,
            assignee_agent_id,
            attempt_fence,
            attempt_controller_generation,
            now,
        )?;
        let binding = load_coding_run_binding_for_attempt(&transaction, task_id, attempt_id)?
            .ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_coding_run_not_found",
                    "AgentTaskAttempt has no prepared CodingAgentRun binding",
                )
            })?;
        if binding.binding_intent_fingerprint != binding_intent_fingerprint {
            return Err(CommunicationStoreError::new(
                "agent_task_coding_run_binding_conflict",
                "CodingAgentRun binding intent does not match the durable Attempt binding",
            ));
        }
        let may_dispatch = matches!(
            binding.dispatch_state,
            AgentTaskCodingRunDispatchState::Prepared | AgentTaskCodingRunDispatchState::NotStarted
        );
        if may_dispatch {
            transaction
                .execute(
                    "UPDATE wc_agent_task_coding_runs
                     SET dispatch_state = 'outcome_unknown', updated_at_unix_ms = MAX(updated_at_unix_ms, ?3)
                     WHERE task_id = ?1 AND attempt_id = ?2
                       AND dispatch_state IN ('prepared', 'not_started')",
                    params![task_id, attempt_id, now],
                )
                .map_err(store_error)?;
        }
        let binding = load_coding_run_binding_for_attempt(&transaction, task_id, attempt_id)?
            .ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_storage_invariant",
                    "AgentTask CodingAgent binding disappeared during dispatch fencing",
                )
            })?;
        transaction.commit().map_err(store_error)?;
        Ok(AgentTaskCodingRunDispatchClaim {
            binding,
            may_dispatch,
        })
    }

    pub fn record_agent_task_coding_run_not_started(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        attempt_id: &str,
        run_id: &str,
    ) -> Result<AgentTaskCodingRunBindingRecord, CommunicationStoreError> {
        let now = now_unix_ms();
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let _task = load_owned_task(&transaction, principal, task_id, now)?;
        let binding = load_coding_run_binding_for_attempt(&transaction, task_id, attempt_id)?
            .ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_coding_run_not_found",
                    "AgentTaskAttempt has no bound CodingAgentRun",
                )
            })?;
        if binding.run_id != run_id {
            return Err(CommunicationStoreError::new(
                "agent_task_coding_run_identity_mismatch",
                "CodingAgentRun does not match the durable AgentTaskAttempt binding",
            ));
        }
        if binding.dispatch_state == AgentTaskCodingRunDispatchState::OutcomeUnknown {
            transaction
                .execute(
                    "UPDATE wc_agent_task_coding_runs
                     SET dispatch_state = 'not_started', updated_at_unix_ms = MAX(updated_at_unix_ms, ?3)
                     WHERE task_id = ?1 AND attempt_id = ?2 AND dispatch_state = 'outcome_unknown'",
                    params![task_id, attempt_id, now],
                )
                .map_err(store_error)?;
        }
        let binding = load_coding_run_binding_for_attempt(&transaction, task_id, attempt_id)?
            .ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_storage_invariant",
                    "AgentTask CodingAgent binding disappeared after NotStarted observation",
                )
            })?;
        transaction.commit().map_err(store_error)?;
        Ok(binding)
    }

    pub fn record_agent_task_coding_run_observation(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        attempt_id: &str,
        observation: &AgentTaskCodingRunObservation,
    ) -> Result<AgentTaskCodingRunBindingRecord, CommunicationStoreError> {
        let now = now_unix_ms();
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let _task = load_owned_task(&transaction, principal, task_id, now)?;
        let binding = load_coding_run_binding_for_attempt(&transaction, task_id, attempt_id)?
            .ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_coding_run_not_found",
                    "AgentTaskAttempt has no bound CodingAgentRun",
                )
            })?;
        let (binding, _) =
            merge_agent_task_coding_run_observation(&transaction, &binding, observation, now)?;
        transaction.commit().map_err(store_error)?;
        Ok(binding)
    }

    pub fn mark_agent_task_coding_run_reconcile_unavailable(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        attempt_id: &str,
    ) -> Result<AgentTaskCodingRunBindingRecord, CommunicationStoreError> {
        let now = now_unix_ms();
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let _task = load_owned_task(&transaction, principal, task_id, now)?;
        let binding = load_coding_run_binding_for_attempt(&transaction, task_id, attempt_id)?
            .ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_coding_run_not_found",
                    "AgentTaskAttempt has no bound CodingAgentRun",
                )
            })?;
        if binding.dispatch_state == AgentTaskCodingRunDispatchState::Bound {
            transaction
                .execute(
                    "UPDATE wc_agent_task_coding_runs
                     SET dispatch_state = 'outcome_unknown', updated_at_unix_ms = MAX(updated_at_unix_ms, ?3)
                     WHERE task_id = ?1 AND attempt_id = ?2 AND dispatch_state = 'bound'",
                    params![task_id, attempt_id, now],
                )
                .map_err(store_error)?;
        }
        let binding = load_coding_run_binding_for_attempt(&transaction, task_id, attempt_id)?
            .ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_storage_invariant",
                    "AgentTask CodingAgent binding disappeared during reconciliation",
                )
            })?;
        transaction.commit().map_err(store_error)?;
        Ok(binding)
    }

    pub fn terminalize_agent_task_coding_run(
        &self,
        principal: &CommunicationPrincipal,
        task_id: &str,
        attempt_id: &str,
        observation: &AgentTaskCodingRunObservation,
        terminal_result: Option<&str>,
        terminal_reason: Option<&str>,
    ) -> Result<AgentTaskCodingRunReconcileMutation, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        let terminal_result = validate_optional_terminal_text(terminal_result, "terminal_result")?;
        let terminal_reason = validate_optional_terminal_text(terminal_reason, "terminal_reason")?;
        let now = now_unix_ms();
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let task = load_owned_task(&transaction, principal, task_id, now)?;
        if task
            .latest_attempt
            .as_ref()
            .map(|attempt| attempt.attempt_id.as_str())
            != Some(attempt_id)
        {
            return Err(CommunicationStoreError::new(
                "agent_task_attempt_stale",
                "Bound CodingAgentRun no longer belongs to the latest authoritative AgentTaskAttempt",
            ));
        }
        let binding = load_coding_run_binding_for_attempt(&transaction, task_id, attempt_id)?
            .ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_coding_run_not_found",
                    "AgentTaskAttempt has no bound CodingAgentRun",
                )
            })?;
        let (binding, disposition) =
            merge_agent_task_coding_run_observation(&transaction, &binding, observation, now)?;
        if disposition == CodingAgentObservationMerge::Stale {
            return Err(CommunicationStoreError::new(
                "agent_task_coding_run_observation_stale",
                "stale CodingAgentRun observation cannot terminalize AgentTask state",
            ));
        }
        let desired_task_state = match binding.last_observed_run_state.as_ref() {
            Some(CodingAgentRunState::Completed) => AgentTaskState::Succeeded,
            Some(CodingAgentRunState::Failed | CodingAgentRunState::Cancelled) => {
                AgentTaskState::Failed
            }
            _ => {
                return Err(CommunicationStoreError::new(
                    "agent_task_coding_run_not_terminal",
                    "CodingAgentRun is not an authoritative terminal result for AgentTask reconciliation",
                ))
            }
        };
        let observation_revision = binding.last_observation_revision.ok_or_else(|| {
            CommunicationStoreError::new(
                "agent_task_storage_invariant",
                "terminal CodingAgent reconciliation lacks an authoritative observation revision",
            )
        })?;
        let attempt =
            load_attempt_for_task(&transaction, task_id, attempt_id, now)?.ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_attempt_not_found",
                    "AgentTaskAttempt does not exist",
                )
            })?;
        if task.stored_state.terminal() {
            let terminal_attempt_id = transaction
                .query_row(
                    "SELECT terminal_attempt_id FROM wc_agent_tasks WHERE task_id = ?1",
                    [task_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .map_err(store_error)?;
            if terminal_attempt_id.as_deref() != Some(attempt_id)
                || task.stored_state != desired_task_state
            {
                return Err(CommunicationStoreError::new(
                    "agent_task_attempt_stale",
                    "AgentTask is already terminal from a different authoritative result",
                ));
            }
            transaction.commit().map_err(store_error)?;
            return Ok(AgentTaskCodingRunReconcileMutation {
                task: task.summary(now),
                attempt: attempt.record(now),
                binding,
                state_changed: false,
            });
        }
        let attempt_state = match desired_task_state {
            AgentTaskState::Succeeded => AgentTaskAttemptState::Succeeded,
            AgentTaskState::Failed => AgentTaskAttemptState::Failed,
            AgentTaskState::Ready | AgentTaskState::Active => unreachable!(),
        };
        transaction
            .execute(
                "UPDATE wc_agent_task_attempts
                 SET state = ?2, terminal_at_unix_ms = ?3,
                     terminal_result = ?4, terminal_reason = ?5
                 WHERE attempt_id = ?1",
                params![
                    attempt_id,
                    attempt_state.as_str(),
                    now,
                    terminal_result,
                    terminal_reason,
                ],
            )
            .map_err(store_error)?;
        transaction
            .execute(
                "UPDATE wc_agent_tasks
                 SET state = ?2, terminal_attempt_id = ?3, terminal_at_unix_ms = ?4,
                     updated_at_unix_ms = MAX(updated_at_unix_ms, ?4)
                 WHERE task_id = ?1",
                params![task_id, desired_task_state.as_str(), attempt_id, now],
            )
            .map_err(store_error)?;
        let binding_updated = transaction
            .execute(
                "UPDATE wc_agent_task_coding_runs
                 SET dispatch_state = 'terminal',
                     terminal_at_unix_ms = ?4,
                     updated_at_unix_ms = MAX(updated_at_unix_ms, ?4)
                 WHERE task_id = ?1 AND attempt_id = ?2 AND last_observation_revision = ?3",
                params![task_id, attempt_id, observation_revision, now],
            )
            .map_err(store_error)?;
        if binding_updated != 1 {
            return Err(CommunicationStoreError::new(
                "agent_task_storage_invariant",
                "terminal CodingAgent binding revision CAS did not update the exact durable binding",
            ));
        }
        let task = load_owned_task(&transaction, principal, task_id, now)?;
        let attempt =
            load_attempt_for_task(&transaction, task_id, attempt_id, now)?.ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_attempt_not_found",
                    "AgentTaskAttempt disappeared after backend reconciliation",
                )
            })?;
        let binding = load_coding_run_binding_for_attempt(&transaction, task_id, attempt_id)?
            .ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_storage_invariant",
                    "AgentTask CodingAgent binding disappeared after terminal reconciliation",
                )
            })?;
        transaction.commit().map_err(store_error)?;
        Ok(AgentTaskCodingRunReconcileMutation {
            task: task.summary(now),
            attempt: attempt.record(now),
            binding,
            state_changed: true,
        })
    }
}

fn require_coding_run_observation_identity(
    binding: &AgentTaskCodingRunBindingRecord,
    observation: &AgentTaskCodingRunObservation,
) -> Result<(), CommunicationStoreError> {
    if binding.run_id != observation.run_id
        || binding.runtime_project_id != observation.runtime_project_id
        || binding.provider_id != observation.provider_id
        || binding.provider_instance_id != observation.provider_instance_id
        || binding.authority_fingerprint != observation.authority_fingerprint
        || binding.coding_agent_intent_fingerprint != observation.coding_agent_intent_fingerprint
    {
        return Err(CommunicationStoreError::new(
            "agent_task_coding_run_identity_mismatch",
            "CodingAgentRun snapshot does not match the exact durable AgentTaskAttempt binding",
        ));
    }
    Ok(())
}

fn canonical_coding_run_snapshot(
    observation: &AgentTaskCodingRunObservation,
) -> Result<CodingAgentRunSnapshot, String> {
    for (label, value) in [
        (
            "terminal_stop_reason",
            observation.terminal_stop_reason.as_deref(),
        ),
        (
            "terminal_error_code",
            observation.terminal_error_code.as_deref(),
        ),
        ("terminal_message", observation.terminal_message.as_deref()),
    ] {
        if let Some(value) = value {
            if value.len() > MAX_AGENT_TASK_TERMINAL_TEXT_BYTES {
                return Err(format!(
                    "{label} exceeds the AgentTask bounded terminal text limit"
                ));
            }
        }
    }

    let state = observation.run_state.clone();
    let execution_state = observation.execution_state;
    let observation_revision = u64::try_from(observation.observation_revision).map_err(|_| {
        "CodingAgentRun snapshot contains a negative observation revision".to_string()
    })?;
    let has_terminal_metadata = observation.terminal_stop_reason.is_some()
        || observation.terminal_error_code.is_some()
        || observation.terminal_message.is_some()
        || observation.completed_at_unix.is_some();
    let terminal = if has_terminal_metadata {
        Some(CodingAgentTerminal {
            stop_reason: observation.terminal_stop_reason.clone(),
            error_code: observation.terminal_error_code.clone(),
            message: observation.terminal_message.clone(),
            completed_at: observation.completed_at_unix.ok_or_else(|| {
                "CodingAgentRun terminal observation lacks completed timestamp".to_string()
            })?,
        })
    } else {
        None
    };
    let snapshot = CodingAgentRunSnapshot {
        run_id: observation.run_id.clone(),
        intent_fingerprint: observation.coding_agent_intent_fingerprint.clone(),
        authority_fingerprint: observation.authority_fingerprint.clone(),
        runtime_project_id: observation.runtime_project_id.clone(),
        provider_id: observation.provider_id.clone(),
        provider_instance_id: observation.provider_instance_id.clone(),
        state,
        execution_state,
        observation_revision,
        // AgentTask persists the authoritative semantic observation fields but not
        // Runner wall-clock snapshot metadata. Neutralize those non-persisted fields
        // so durable replay/conflict classification compares exactly what is stored.
        created_at: 0,
        updated_at: 0,
        terminal,
    };
    validate_coding_agent_run_snapshot(&snapshot)?;
    Ok(snapshot)
}

fn validate_coding_run_observation(
    observation: &AgentTaskCodingRunObservation,
) -> Result<CodingAgentRunSnapshot, CommunicationStoreError> {
    canonical_coding_run_snapshot(observation).map_err(|message| {
        CommunicationStoreError::new("invalid_agent_task_coding_run_observation", message)
    })
}

fn stored_coding_run_snapshot(
    binding: &AgentTaskCodingRunBindingRecord,
) -> Result<Option<CodingAgentRunSnapshot>, CommunicationStoreError> {
    let Some(revision) = binding.last_observation_revision else {
        if binding.last_observed_run_state.is_some()
            || binding.last_observed_execution_state.is_some()
            || binding.terminal_stop_reason.is_some()
            || binding.terminal_error_code.is_some()
            || binding.terminal_message.is_some()
            || binding.completed_at_unix.is_some()
        {
            return Err(CommunicationStoreError::new(
                "agent_task_storage_invariant",
                "CodingAgent binding has observation fields without an observation revision",
            ));
        }
        return Ok(None);
    };
    let observation = AgentTaskCodingRunObservation {
        run_id: binding.run_id.clone(),
        runtime_project_id: binding.runtime_project_id.clone(),
        provider_id: binding.provider_id.clone(),
        provider_instance_id: binding.provider_instance_id.clone(),
        authority_fingerprint: binding.authority_fingerprint.clone(),
        coding_agent_intent_fingerprint: binding.coding_agent_intent_fingerprint.clone(),
        run_state: binding.last_observed_run_state.clone().ok_or_else(|| {
            CommunicationStoreError::new(
                "agent_task_storage_invariant",
                "CodingAgent binding observation revision lacks run state",
            )
        })?,
        execution_state: binding
            .last_observed_execution_state
            .clone()
            .ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_storage_invariant",
                    "CodingAgent binding observation revision lacks execution state",
                )
            })?,
        observation_revision: revision,
        terminal_stop_reason: binding.terminal_stop_reason.clone(),
        terminal_error_code: binding.terminal_error_code.clone(),
        terminal_message: binding.terminal_message.clone(),
        completed_at_unix: binding.completed_at_unix,
    };
    canonical_coding_run_snapshot(&observation)
        .map(Some)
        .map_err(|message| {
            CommunicationStoreError::new(
                "agent_task_storage_invariant",
                format!("stored CodingAgent observation is invalid: {message}"),
            )
        })
}

fn merge_agent_task_coding_run_observation(
    transaction: &Transaction<'_>,
    binding: &AgentTaskCodingRunBindingRecord,
    observation: &AgentTaskCodingRunObservation,
    now: i64,
) -> Result<(AgentTaskCodingRunBindingRecord, CodingAgentObservationMerge), CommunicationStoreError>
{
    require_coding_run_observation_identity(binding, observation)?;
    let incoming = validate_coding_run_observation(observation)?;
    let disposition = match stored_coding_run_snapshot(binding)? {
        Some(stored) => merge_coding_agent_run_snapshot(&stored, &incoming).map_err(|message| {
            CommunicationStoreError::new("agent_task_coding_run_observation_conflict", message)
        })?,
        None => CodingAgentObservationMerge::Advance,
    };
    if matches!(
        disposition,
        CodingAgentObservationMerge::Stale | CodingAgentObservationMerge::ExactReplay
    ) {
        return Ok((binding.clone(), disposition));
    }

    let dispatch_state = if observation.run_state == CodingAgentRunState::Lost
        || observation.execution_state == CodingAgentExecutionState::OutcomeUnknown
    {
        AgentTaskCodingRunDispatchState::OutcomeUnknown
    } else {
        AgentTaskCodingRunDispatchState::Bound
    };
    let updated = if let Some(expected_revision) = binding.last_observation_revision {
        transaction
            .execute(
                "UPDATE wc_agent_task_coding_runs
                 SET dispatch_state = ?3,
                     last_observed_run_state = ?4,
                     last_observed_execution_state = ?5,
                     last_observation_revision = ?6,
                     terminal_stop_reason = ?7,
                     terminal_error_code = ?8,
                     terminal_message = ?9,
                     completed_at_unix = ?10,
                     updated_at_unix_ms = MAX(updated_at_unix_ms, ?11)
                 WHERE task_id = ?1 AND attempt_id = ?2 AND last_observation_revision = ?12",
                params![
                    binding.task_id,
                    binding.attempt_id,
                    dispatch_state.as_str(),
                    observation.run_state.as_str(),
                    observation.execution_state.as_str(),
                    observation.observation_revision,
                    observation.terminal_stop_reason,
                    observation.terminal_error_code,
                    observation.terminal_message,
                    observation.completed_at_unix,
                    now,
                    expected_revision,
                ],
            )
            .map_err(store_error)?
    } else {
        transaction
            .execute(
                "UPDATE wc_agent_task_coding_runs
                 SET dispatch_state = ?3,
                     last_observed_run_state = ?4,
                     last_observed_execution_state = ?5,
                     last_observation_revision = ?6,
                     terminal_stop_reason = ?7,
                     terminal_error_code = ?8,
                     terminal_message = ?9,
                     completed_at_unix = ?10,
                     updated_at_unix_ms = MAX(updated_at_unix_ms, ?11)
                 WHERE task_id = ?1 AND attempt_id = ?2 AND last_observation_revision IS NULL",
                params![
                    binding.task_id,
                    binding.attempt_id,
                    dispatch_state.as_str(),
                    observation.run_state.as_str(),
                    observation.execution_state.as_str(),
                    observation.observation_revision,
                    observation.terminal_stop_reason,
                    observation.terminal_error_code,
                    observation.terminal_message,
                    observation.completed_at_unix,
                    now,
                ],
            )
            .map_err(store_error)?
    };
    if updated != 1 {
        return Err(CommunicationStoreError::new(
            "agent_task_storage_invariant",
            "CodingAgent observation revision CAS did not update the exact durable binding",
        ));
    }
    let binding =
        load_coding_run_binding_for_attempt(transaction, &binding.task_id, &binding.attempt_id)?
            .ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_storage_invariant",
                    "AgentTask CodingAgent binding disappeared after monotonic observation merge",
                )
            })?;
    Ok((binding, disposition))
}

fn task_request_hash(value: &serde_json::Value) -> String {
    digest_text(
        "webcodex.agent-task.request.v1",
        &serde_json::to_string(value).expect("AgentTask request serializes"),
    )
}

fn validate_title(value: &str) -> Result<String, CommunicationStoreError> {
    let value = value.trim();
    let chars = value.chars().count();
    if chars == 0 || chars > MAX_AGENT_TASK_TITLE_CHARS {
        return Err(CommunicationStoreError::new(
            "invalid_agent_task_title",
            format!("title must contain 1..={MAX_AGENT_TASK_TITLE_CHARS} characters"),
        ));
    }
    Ok(value.to_string())
}

fn validate_instruction(value: &str) -> Result<String, CommunicationStoreError> {
    let value = value.trim();
    let bytes = value.len();
    if bytes == 0 || bytes > MAX_AGENT_TASK_INSTRUCTION_BYTES {
        return Err(CommunicationStoreError::new(
            "invalid_agent_task_instruction",
            format!("instruction must contain 1..={MAX_AGENT_TASK_INSTRUCTION_BYTES} UTF-8 bytes"),
        ));
    }
    Ok(value.to_string())
}

fn validate_project_reference(
    value: Option<&str>,
) -> Result<Option<String>, CommunicationStoreError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    let chars = value.chars().count();
    if chars == 0 || chars > MAX_AGENT_TASK_PROJECT_REF_CHARS {
        return Err(CommunicationStoreError::new(
            "invalid_agent_task_project_reference",
            format!(
                "referenced_project_id must contain 1..={MAX_AGENT_TASK_PROJECT_REF_CHARS} characters"
            ),
        ));
    }
    Ok(Some(value.to_string()))
}

fn validate_optional_terminal_text(
    value: Option<&str>,
    label: &str,
) -> Result<Option<String>, CommunicationStoreError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_AGENT_TASK_TERMINAL_TEXT_BYTES {
        return Err(CommunicationStoreError::new(
            "invalid_agent_task_terminal_text",
            format!(
                "{label} must contain 1..={MAX_AGENT_TASK_TERMINAL_TEXT_BYTES} UTF-8 bytes when provided"
            ),
        ));
    }
    Ok(Some(value.to_string()))
}

fn validate_optional_id(
    value: Option<&str>,
    prefix: &str,
    code: &'static str,
) -> Result<Option<String>, CommunicationStoreError> {
    let Some(value) = value else {
        return Ok(None);
    };
    validate_id(value, prefix, code)?;
    Ok(Some(value.to_string()))
}

fn require_owned_agent(
    conn: &Connection,
    principal: &CommunicationPrincipal,
    agent_id: &str,
) -> Result<(), CommunicationStoreError> {
    validate_id(agent_id, DURABLE_AGENT_ID_PREFIX, "invalid_agent_id")?;
    let owned = conn
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM wc_agent_identities
                WHERE agent_id = ?1 AND owner_principal_kind = ?2 AND owner_principal_digest = ?3
             )",
            params![agent_id, principal.kind, principal.digest],
            |row| row.get::<_, bool>(0),
        )
        .map_err(store_error)?;
    if !owned {
        return Err(CommunicationStoreError::new(
            "agent_not_found",
            "Agent identity does not exist",
        ));
    }
    Ok(())
}

fn validate_source_references(
    conn: &Connection,
    principal: &CommunicationPrincipal,
    conversation_id: Option<&str>,
    message_id: Option<&str>,
) -> Result<(), CommunicationStoreError> {
    let Some(conversation_id) = conversation_id else {
        return Ok(());
    };
    authorize_conversation_access(conn, principal, &ConversationAccess::Human, conversation_id)?;
    if let Some(message_id) = message_id {
        let exists = conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM wc_conversation_messages
                    WHERE conversation_id = ?1 AND message_id = ?2
                 )",
                params![conversation_id, message_id],
                |row| row.get::<_, bool>(0),
            )
            .map_err(store_error)?;
        if !exists {
            return Err(CommunicationStoreError::new(
                "agent_task_source_message_not_found",
                "Conversation Message does not exist in the referenced Conversation",
            ));
        }
    }
    Ok(())
}

fn load_owned_task(
    conn: &Connection,
    principal: &CommunicationPrincipal,
    task_id: &str,
    now: i64,
) -> Result<StoredTask, CommunicationStoreError> {
    validate_id(task_id, AGENT_TASK_ID_PREFIX, "invalid_agent_task_id")?;
    let row = conn
        .query_row(
            "SELECT task_id, assignee_agent_id, title, instruction,
                    source_conversation_id, source_message_id, referenced_project_id,
                    state, latest_attempt_id, created_at_unix_ms, updated_at_unix_ms,
                    terminal_at_unix_ms
             FROM wc_agent_tasks
             WHERE task_id = ?1 AND owner_principal_kind = ?2 AND owner_principal_digest = ?3",
            params![task_id, principal.kind, principal.digest],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    AgentTaskState::from_db(&row.get::<_, String>(7)?, 7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, Option<i64>>(11)?,
                ))
            },
        )
        .optional()
        .map_err(store_error)?
        .ok_or_else(|| {
            CommunicationStoreError::new("agent_task_not_found", "AgentTask does not exist")
        })?;
    let latest_attempt = match row.8.as_deref() {
        Some(attempt_id) => {
            load_attempt_for_task(conn, task_id, attempt_id, now)?.ok_or_else(|| {
                CommunicationStoreError::new(
                    "agent_task_storage_invariant",
                    "AgentTask latest Attempt is missing",
                )
            })?
        }
        None => {
            return Ok(StoredTask {
                task_id: row.0,
                assignee_agent_id: row.1,
                title: row.2,
                instruction: row.3,
                source_conversation_id: row.4,
                source_message_id: row.5,
                referenced_project_id: row.6,
                stored_state: row.7,
                created_at_unix_ms: row.9,
                updated_at_unix_ms: row.10,
                terminal_at_unix_ms: row.11,
                latest_attempt: None,
                latest_coding_run: None,
            })
        }
    };
    let latest_coding_run =
        load_coding_run_binding_for_attempt(conn, task_id, &latest_attempt.attempt_id)?;
    Ok(StoredTask {
        task_id: row.0,
        assignee_agent_id: row.1,
        title: row.2,
        instruction: row.3,
        source_conversation_id: row.4,
        source_message_id: row.5,
        referenced_project_id: row.6,
        stored_state: row.7,
        created_at_unix_ms: row.9,
        updated_at_unix_ms: row.10,
        terminal_at_unix_ms: row.11,
        latest_attempt: Some(latest_attempt),
        latest_coding_run,
    })
}

fn load_attempt_for_task(
    conn: &Connection,
    task_id: &str,
    attempt_id: &str,
    _now: i64,
) -> Result<Option<StoredAttempt>, CommunicationStoreError> {
    validate_id(
        attempt_id,
        AGENT_TASK_ATTEMPT_ID_PREFIX,
        "invalid_agent_task_attempt_id",
    )?;
    conn.query_row(
        "SELECT attempt_id, task_id, attempt_number, assignee_agent_id, state,
                lease_expires_at_unix_ms, attempt_fence, attempt_controller_generation,
                created_at_unix_ms, started_at_unix_ms, terminal_at_unix_ms,
                terminal_result, terminal_reason
         FROM wc_agent_task_attempts
         WHERE attempt_id = ?1 AND task_id = ?2",
        params![attempt_id, task_id],
        |row| {
            Ok(StoredAttempt {
                attempt_id: row.get(0)?,
                task_id: row.get(1)?,
                attempt_number: row.get(2)?,
                assignee_agent_id: row.get(3)?,
                stored_state: AgentTaskAttemptState::from_db(&row.get::<_, String>(4)?, 4)?,
                lease_expires_at_unix_ms: row.get(5)?,
                attempt_fence: row.get(6)?,
                attempt_controller_generation: row.get(7)?,
                created_at_unix_ms: row.get(8)?,
                started_at_unix_ms: row.get(9)?,
                terminal_at_unix_ms: row.get(10)?,
                terminal_result: row.get(11)?,
                terminal_reason: row.get(12)?,
            })
        },
    )
    .optional()
    .map_err(store_error)
}

fn load_coding_run_binding_for_attempt(
    conn: &Connection,
    task_id: &str,
    attempt_id: &str,
) -> Result<Option<AgentTaskCodingRunBindingRecord>, CommunicationStoreError> {
    conn.query_row(
        "SELECT task_id, attempt_id, run_id, runtime_project_id, provider_id,
                provider_instance_id, authority_fingerprint, coding_agent_intent_fingerprint,
                binding_intent_fingerprint, dispatch_state, last_observed_run_state,
                last_observed_execution_state, last_observation_revision,
                terminal_stop_reason, terminal_error_code, terminal_message,
                completed_at_unix, created_at_unix_ms, updated_at_unix_ms, terminal_at_unix_ms
         FROM wc_agent_task_coding_runs
         WHERE task_id = ?1 AND attempt_id = ?2",
        params![task_id, attempt_id],
        |row| {
            Ok(AgentTaskCodingRunBindingRecord {
                task_id: row.get(0)?,
                attempt_id: row.get(1)?,
                run_id: row.get(2)?,
                runtime_project_id: row.get(3)?,
                provider_id: row.get(4)?,
                provider_instance_id: row.get(5)?,
                authority_fingerprint: row.get(6)?,
                coding_agent_intent_fingerprint: row.get(7)?,
                binding_intent_fingerprint: row.get(8)?,
                dispatch_state: AgentTaskCodingRunDispatchState::from_db(
                    &row.get::<_, String>(9)?,
                    9,
                )?,
                last_observed_run_state: row
                    .get::<_, Option<String>>(10)?
                    .as_deref()
                    .map(|value| {
                        CodingAgentRunState::from_str(value).ok_or_else(|| {
                            rusqlite::Error::FromSqlConversionFailure(
                                10,
                                Type::Text,
                                format!("unsupported observed CodingAgentRun state: {value}")
                                    .into(),
                            )
                        })
                    })
                    .transpose()?,
                last_observed_execution_state: row
                    .get::<_, Option<String>>(11)?
                    .as_deref()
                    .map(|value| {
                        CodingAgentExecutionState::from_str(value).ok_or_else(|| {
                            rusqlite::Error::FromSqlConversionFailure(
                                11,
                                Type::Text,
                                format!(
                                    "unsupported observed CodingAgent execution state: {value}"
                                )
                                .into(),
                            )
                        })
                    })
                    .transpose()?,
                last_observation_revision: row.get(12)?,
                terminal_stop_reason: row.get(13)?,
                terminal_error_code: row.get(14)?,
                terminal_message: row.get(15)?,
                completed_at_unix: row.get(16)?,
                created_at_unix_ms: row.get(17)?,
                updated_at_unix_ms: row.get(18)?,
                terminal_at_unix_ms: row.get(19)?,
            })
        },
    )
    .optional()
    .map_err(store_error)
}

fn blocking_execution_error(
    binding: Option<&AgentTaskCodingRunBindingRecord>,
) -> Option<CommunicationStoreError> {
    let binding = binding.filter(|binding| binding.dispatch_state.blocks_replacement())?;
    if binding.dispatch_state == AgentTaskCodingRunDispatchState::OutcomeUnknown
        || binding.last_observed_run_state.as_ref() == Some(&CodingAgentRunState::Lost)
    {
        Some(CommunicationStoreError::new(
            "agent_task_execution_outcome_unknown",
            "AgentTask has a CodingAgent execution whose outcome is unresolved; reconcile the exact bound Run before replacement",
        ))
    } else {
        Some(CommunicationStoreError::new(
            "agent_task_execution_active",
            "AgentTask has a bound CodingAgent execution that must be reconciled before replacement",
        ))
    }
}

fn materialize_expired_latest_attempt(
    transaction: &Transaction<'_>,
    task: &mut StoredTask,
    now: i64,
) -> Result<(), CommunicationStoreError> {
    let Some(attempt) = task.latest_attempt.as_mut() else {
        return Ok(());
    };
    if attempt.stored_state != AgentTaskAttemptState::Active
        || attempt.lease_expires_at_unix_ms > now
    {
        return Ok(());
    }
    transaction
        .execute(
            "UPDATE wc_agent_task_attempts
             SET state = 'expired', terminal_at_unix_ms = ?2
             WHERE attempt_id = ?1 AND state = 'active'",
            params![attempt.attempt_id, now],
        )
        .map_err(store_error)?;
    let execution_blocks_replacement = task
        .latest_coding_run
        .as_ref()
        .is_some_and(|binding| binding.dispatch_state.blocks_replacement());
    if !execution_blocks_replacement {
        transaction
            .execute(
                "UPDATE wc_agent_tasks
                 SET state = 'ready', updated_at_unix_ms = MAX(updated_at_unix_ms, ?2)
                 WHERE task_id = ?1 AND state = 'active'",
                params![task.task_id, now],
            )
            .map_err(store_error)?;
        task.stored_state = AgentTaskState::Ready;
    }
    attempt.stored_state = AgentTaskAttemptState::Expired;
    attempt.terminal_at_unix_ms = Some(now);
    task.updated_at_unix_ms = task.updated_at_unix_ms.max(now);
    Ok(())
}

fn validate_attempt_mutation_inputs(
    principal: &CommunicationPrincipal,
    task_id: &str,
    attempt_id: &str,
    assignee_agent_id: &str,
    attempt_fence: &str,
    attempt_controller_generation: i64,
) -> Result<(), CommunicationStoreError> {
    validate_communication_principal(principal)?;
    validate_id(task_id, AGENT_TASK_ID_PREFIX, "invalid_agent_task_id")?;
    validate_id(
        attempt_id,
        AGENT_TASK_ATTEMPT_ID_PREFIX,
        "invalid_agent_task_attempt_id",
    )?;
    validate_id(
        assignee_agent_id,
        DURABLE_AGENT_ID_PREFIX,
        "invalid_agent_id",
    )?;
    validate_id(
        attempt_fence,
        AGENT_TASK_ATTEMPT_FENCE_PREFIX,
        "invalid_agent_task_attempt_fence",
    )?;
    if attempt_controller_generation < 1 {
        return Err(CommunicationStoreError::new(
            "invalid_agent_task_attempt_controller_generation",
            "attempt_controller_generation must be at least 1",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn require_current_attempt(
    conn: &Connection,
    task: &StoredTask,
    attempt_id: &str,
    assignee_agent_id: &str,
    attempt_fence: &str,
    attempt_controller_generation: i64,
    now: i64,
) -> Result<StoredAttempt, CommunicationStoreError> {
    if task.stored_state.terminal() {
        return Err(task_terminal_error());
    }
    if task
        .latest_attempt
        .as_ref()
        .map(|attempt| attempt.attempt_id.as_str())
        != Some(attempt_id)
    {
        return Err(CommunicationStoreError::new(
            "agent_task_attempt_stale",
            "AgentTaskAttempt is not the latest authoritative Attempt",
        ));
    }
    if task.assignee_agent_id.as_deref() != Some(assignee_agent_id) {
        return Err(CommunicationStoreError::new(
            "agent_task_assignee_mismatch",
            "AgentTask current assignee does not match the Attempt caller assertion",
        ));
    }
    let attempt =
        load_attempt_for_task(conn, &task.task_id, attempt_id, now)?.ok_or_else(|| {
            CommunicationStoreError::new(
                "agent_task_attempt_not_found",
                "AgentTaskAttempt does not exist",
            )
        })?;
    if attempt.assignee_agent_id != assignee_agent_id
        || attempt.attempt_fence != attempt_fence
        || attempt.attempt_controller_generation != attempt_controller_generation
    {
        return Err(CommunicationStoreError::new(
            "agent_task_attempt_stale",
            "AgentTaskAttempt fence, assignee, or controller generation is stale",
        ));
    }
    if attempt.stored_state != AgentTaskAttemptState::Active
        || attempt.lease_expires_at_unix_ms <= now
    {
        return Err(CommunicationStoreError::new(
            "agent_task_attempt_stale",
            "AgentTaskAttempt is terminal, superseded, or its lease has expired",
        ));
    }
    Ok(attempt)
}

fn task_terminal_error() -> CommunicationStoreError {
    CommunicationStoreError::new(
        "agent_task_terminal",
        "AgentTask is terminal and cannot start or mutate execution ownership",
    )
}
