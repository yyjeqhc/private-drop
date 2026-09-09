use super::agent_wake::{
    coalesce_agent_wake_for_delivery, reconcile_wakes_for_endpoint_loss, AgentWakeState,
};
use super::Database;
use rusqlite::{
    params, types::Type, Connection, OptionalExtension, Transaction, TransactionBehavior,
};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use uuid::Uuid;

pub(crate) const DURABLE_AGENT_ID_PREFIX: &str = "wc_dagent_";
pub(crate) const AGENT_ENDPOINT_ID_PREFIX: &str = "wc_endpoint_";
pub(crate) const CONVERSATION_ID_PREFIX: &str = "wc_conv_";
pub(crate) const CONVERSATION_PARTICIPANT_ID_PREFIX: &str = "wc_participant_";
pub(crate) const CONVERSATION_MESSAGE_ID_PREFIX: &str = "wc_cmsg_";
pub(crate) const AGENT_DELIVERY_ID_PREFIX: &str = "wc_delivery_";
pub const COMMUNICATION_PRINCIPAL_DIGEST_PREFIX: &str = "wc_commprincipal_";

pub(crate) const MAX_AGENT_HANDLE_CHARS: usize = 64;
pub(crate) const MAX_AGENT_DISPLAY_NAME_CHARS: usize = 128;
pub(crate) const MAX_AGENT_DESCRIPTION_BYTES: usize = 2_048;
pub(crate) const MAX_AGENT_SPECIALTY_LABELS: usize = 16;
pub(crate) const MAX_AGENT_SPECIALTY_LABEL_CHARS: usize = 64;
pub(crate) const MAX_ENDPOINT_HOST_CHARS: usize = 64;
pub(crate) const MAX_ENDPOINT_ATTACHMENT_CHARS: usize = 128;
pub(crate) const MAX_CONVERSATION_TITLE_CHARS: usize = 200;
pub(crate) const MAX_CONVERSATION_AGENT_PARTICIPANTS: usize = 16;
pub(crate) const MAX_CONVERSATION_MESSAGE_BYTES: usize = 4_096;
pub(crate) const MAX_COMMUNICATION_IDEMPOTENCY_KEY_CHARS: usize = 128;
pub(crate) const MAX_COMMUNICATION_LIST_LIMIT: usize = 100;
pub(crate) const MAX_DELIVERY_CONSUME_ITEMS: usize = 100;
const MAX_COMMUNICATION_PRINCIPAL_KIND_CHARS: usize = 64;

pub(crate) const DEFAULT_ENDPOINT_LEASE_MS: i64 = 120_000;

pub const MAX_DURABLE_AGENTS: i64 = 4_096;
const MAX_CONVERSATIONS: i64 = 8_192;
const MAX_MESSAGES_PER_CONVERSATION: i64 = 100_000;

const OP_CREATE_AGENT: &str = "create_agent_identity";
const OP_ATTACH_ENDPOINT: &str = "attach_agent_endpoint";
const OP_CREATE_CONVERSATION: &str = "create_conversation";
const OP_POST_MESSAGE: &str = "post_conversation_message";
const OP_POST_WAKE_REPLY: &str = "post_agent_wake_reply";
const MAX_WAKE_REPLY_OPERATION_INDEX: i64 = 31;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommunicationStoreError {
    code: &'static str,
    message: String,
    current_profile_revision: Option<i64>,
}

impl CommunicationStoreError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            current_profile_revision: None,
        }
    }

    fn profile_changed(current_profile_revision: i64) -> Self {
        Self {
            code: "agent_profile_changed",
            message: format!(
                "Agent profile changed; current profile revision is {current_profile_revision}"
            ),
            current_profile_revision: Some(current_profile_revision),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn current_profile_revision(&self) -> Option<i64> {
        self.current_profile_revision
    }
}

impl std::fmt::Display for CommunicationStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CommunicationStoreError {}

pub(super) fn store_error(error: rusqlite::Error) -> CommunicationStoreError {
    tracing::warn!(error = %error, "durable communication store operation failed");
    CommunicationStoreError::new(
        "communication_store_unavailable",
        "Durable communication store is unavailable",
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommunicationPrincipal {
    pub kind: String,
    pub digest: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentEndpointLifecycle {
    Attached,
    Detached,
    Expired,
}

impl AgentEndpointLifecycle {
    pub const fn as_db(self) -> &'static str {
        match self {
            Self::Attached => "attached",
            Self::Detached => "detached",
            Self::Expired => "expired",
        }
    }

    fn from_db(value: &str, index: usize) -> rusqlite::Result<Self> {
        match value {
            "attached" => Ok(Self::Attached),
            "detached" => Ok(Self::Detached),
            "expired" => Ok(Self::Expired),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                Type::Text,
                format!("unsupported Agent Endpoint lifecycle: {other}").into(),
            )),
        }
    }
}

impl std::fmt::Display for AgentEndpointLifecycle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_db())
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationLifecycle {
    Open,
    Closed,
}

impl ConversationLifecycle {
    pub const fn as_db(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }

    fn from_db(value: &str, index: usize) -> rusqlite::Result<Self> {
        match value {
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                Type::Text,
                format!("unsupported Conversation lifecycle: {other}").into(),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageDeliveryState {
    Queued,
    Consumed,
}

impl MessageDeliveryState {
    pub const fn as_db(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Consumed => "consumed",
        }
    }

    fn from_db(value: &str, index: usize) -> rusqlite::Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "consumed" => Ok(Self::Consumed),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                Type::Text,
                format!("unsupported Message Delivery state: {other}").into(),
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewAgentIdentity {
    pub handle: String,
    pub display_name: String,
    pub description: String,
    pub specialty_labels: Vec<String>,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Default)]
pub struct AgentProfilePatch {
    pub handle: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub specialty_labels: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct NewAgentEndpoint {
    pub agent_id: String,
    pub host: String,
    pub client_attachment_id: Option<String>,
    pub wake_capable: bool,
    pub idempotency_key: String,
}

#[derive(Debug, Clone)]
pub struct NewConversation {
    pub title: Option<String>,
    pub agent_ids: Vec<String>,
    pub idempotency_key: String,
}

#[derive(Debug, Clone)]
pub enum ConversationAccess {
    Human,
    Agent {
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
    },
}

#[derive(Debug, Clone)]
pub struct NewConversationMessage {
    pub conversation_id: String,
    pub body: String,
    pub author_agent_id: Option<String>,
    pub endpoint_id: Option<String>,
    pub expected_controller_generation: Option<i64>,
    /// None means every Agent participant except the Agent author. Some([])
    /// means transcript-only delivery to the room/human participants.
    pub recipient_agent_ids: Option<Vec<String>>,
    pub reply_to: Option<String>,
    pub idempotency_key: Option<String>,
    /// Stable automatic/manual resumed-turn reply identity. The store derives
    /// the idempotency identity from the exact Wake plus this bounded operation
    /// index; callers never need to invent a fresh key after an uncertain reply.
    pub wake_reply_id: Option<String>,
    pub reply_operation_index: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DurableAgentIdentity {
    pub agent_id: String,
    pub handle: String,
    pub display_name: String,
    pub description: String,
    pub specialty_labels: Vec<String>,
    pub profile_revision: i64,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
    pub current_controller_generation: i64,
    pub active_endpoint_count: i64,
    pub queued_delivery_count: i64,
    pub unresolved_wake_count: i64,
    pub latest_wake_id: Option<String>,
    pub latest_wake_state: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentIdentityMutation {
    pub agent: DurableAgentIdentity,
    pub created: bool,
    pub replayed: bool,
    pub state_changed: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentIdentityPage {
    pub total_count: i64,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub truncated: bool,
    pub agents: Vec<DurableAgentIdentity>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentEndpointRecord {
    pub endpoint_id: String,
    pub agent_id: String,
    pub host: String,
    pub client_attachment_id: Option<String>,
    pub wake_capable: bool,
    pub controller_generation: i64,
    pub lifecycle: AgentEndpointLifecycle,
    pub attached_at_unix_ms: i64,
    pub last_seen_at_unix_ms: i64,
    pub lease_expires_at_unix_ms: i64,
    pub expired_at_unix_ms: Option<i64>,
    pub detached_at_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentEndpointMutation {
    pub endpoint: AgentEndpointRecord,
    pub created: bool,
    pub replayed: bool,
    pub state_changed: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConversationParticipantRecord {
    pub participant_id: String,
    pub participant_kind: String,
    pub agent_id: Option<String>,
    pub handle: Option<String>,
    pub display_name: Option<String>,
    pub principal_kind: Option<String>,
    pub joined_at_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MessageAuthorRecord {
    pub participant_kind: String,
    pub agent_id: Option<String>,
    pub handle: Option<String>,
    pub display_name: Option<String>,
    pub principal_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MessageDeliveryRecord {
    pub delivery_order: i64,
    pub delivery_id: String,
    pub recipient_agent_id: String,
    pub state: MessageDeliveryState,
    pub created_at_unix_ms: i64,
    pub consumed_at_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConversationMessageRecord {
    pub message_id: String,
    pub conversation_id: String,
    pub seq: i64,
    pub author: MessageAuthorRecord,
    pub body: String,
    pub reply_to: Option<String>,
    pub created_at_unix_ms: i64,
    pub deliveries: Vec<MessageDeliveryRecord>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConversationSummaryRecord {
    pub conversation_id: String,
    pub title: Option<String>,
    pub lifecycle: ConversationLifecycle,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
    pub participant_count: i64,
    pub message_count: i64,
    pub last_seq: i64,
    pub queued_delivery_count: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConversationPage {
    pub total_count: i64,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub truncated: bool,
    pub conversations: Vec<ConversationSummaryRecord>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConversationDetailRecord {
    pub conversation: ConversationSummaryRecord,
    pub participants: Vec<ConversationParticipantRecord>,
    pub messages: Vec<ConversationMessageRecord>,
    pub after_seq: i64,
    pub next_after_seq: Option<i64>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConversationMutation {
    pub conversation: ConversationDetailRecord,
    pub created: bool,
    pub replayed: bool,
    pub state_changed: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConversationMessageMutation {
    pub message: ConversationMessageRecord,
    pub replayed: bool,
    pub state_changed: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentInboxItem {
    pub delivery_id: String,
    pub state: MessageDeliveryState,
    pub conversation_id: String,
    pub conversation_title: Option<String>,
    pub message: ConversationMessageRecord,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentInboxPage {
    pub agent_id: String,
    pub total_queued_count: i64,
    pub after_delivery_order: i64,
    pub next_after_delivery_order: Option<i64>,
    pub truncated: bool,
    pub deliveries: Vec<AgentInboxItem>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeliveryConsumeResult {
    pub agent_id: String,
    pub consumed_delivery_ids: Vec<String>,
    pub already_consumed_delivery_ids: Vec<String>,
    pub state_changed: bool,
}

impl Database {
    pub(super) fn ensure_communication_schema(conn: &mut Connection) -> anyhow::Result<()> {
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS wc_agent_identities (
                agent_id TEXT PRIMARY KEY,
                owner_principal_kind TEXT NOT NULL,
                owner_principal_digest TEXT NOT NULL,
                handle TEXT NOT NULL,
                display_name TEXT NOT NULL,
                description TEXT NOT NULL,
                specialty_labels_json TEXT NOT NULL,
                profile_revision INTEGER NOT NULL CHECK(profile_revision >= 1),
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                current_controller_generation INTEGER NOT NULL DEFAULT 0 CHECK(current_controller_generation >= 0)
            );
            CREATE INDEX IF NOT EXISTS idx_wc_agent_identities_updated
                ON wc_agent_identities(updated_at_unix_ms DESC, agent_id);
            CREATE INDEX IF NOT EXISTS idx_wc_agent_identities_owner
                ON wc_agent_identities(owner_principal_digest, agent_id);

            CREATE TABLE IF NOT EXISTS wc_agent_endpoints (
                endpoint_id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                attachment_principal_kind TEXT NOT NULL,
                attachment_principal_digest TEXT NOT NULL,
                host TEXT NOT NULL,
                client_attachment_id TEXT,
                wake_capable INTEGER NOT NULL CHECK(wake_capable IN (0, 1)),
                controller_generation INTEGER NOT NULL CHECK(controller_generation >= 0),
                lifecycle TEXT NOT NULL CHECK(lifecycle IN ('attached', 'detached', 'expired')),
                attached_at_unix_ms INTEGER NOT NULL,
                last_seen_at_unix_ms INTEGER NOT NULL,
                lease_expires_at_unix_ms INTEGER NOT NULL,
                expired_at_unix_ms INTEGER,
                detached_at_unix_ms INTEGER,
                CHECK(
                    (lifecycle = 'attached' AND expired_at_unix_ms IS NULL AND detached_at_unix_ms IS NULL)
                    OR (lifecycle = 'expired' AND expired_at_unix_ms IS NOT NULL)
                    OR (lifecycle = 'detached' AND detached_at_unix_ms IS NOT NULL)
                ),
                FOREIGN KEY(agent_id) REFERENCES wc_agent_identities(agent_id)
            );
            CREATE INDEX IF NOT EXISTS idx_wc_agent_endpoints_agent_active
                ON wc_agent_endpoints(agent_id, detached_at_unix_ms, attached_at_unix_ms DESC);
            CREATE INDEX IF NOT EXISTS idx_wc_agent_endpoints_agent_generation
                ON wc_agent_endpoints(agent_id, lifecycle, controller_generation DESC);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_wc_agent_endpoints_one_attached
                ON wc_agent_endpoints(agent_id) WHERE lifecycle = 'attached';

            CREATE TABLE IF NOT EXISTS wc_conversations (
                conversation_id TEXT PRIMARY KEY,
                title TEXT,
                lifecycle TEXT NOT NULL CHECK(lifecycle IN ('open', 'closed')),
                next_seq INTEGER NOT NULL CHECK(next_seq >= 1),
                created_by_principal_kind TEXT NOT NULL,
                created_by_principal_digest TEXT NOT NULL,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                closed_at_unix_ms INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_wc_conversations_updated
                ON wc_conversations(updated_at_unix_ms DESC, conversation_id);

            CREATE TABLE IF NOT EXISTS wc_conversation_participants (
                participant_id TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL,
                participant_kind TEXT NOT NULL CHECK(participant_kind IN ('human', 'agent')),
                agent_id TEXT,
                principal_kind TEXT,
                principal_digest TEXT,
                joined_at_unix_ms INTEGER NOT NULL,
                CHECK(
                    (participant_kind = 'agent'
                        AND agent_id IS NOT NULL
                        AND principal_kind IS NULL
                        AND principal_digest IS NULL)
                    OR
                    (participant_kind = 'human'
                        AND agent_id IS NULL
                        AND principal_kind IS NOT NULL
                        AND principal_digest IS NOT NULL)
                ),
                FOREIGN KEY(conversation_id) REFERENCES wc_conversations(conversation_id),
                FOREIGN KEY(agent_id) REFERENCES wc_agent_identities(agent_id)
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_wc_conversation_participants_agent
                ON wc_conversation_participants(conversation_id, agent_id)
                WHERE participant_kind = 'agent';
            CREATE UNIQUE INDEX IF NOT EXISTS idx_wc_conversation_participants_human
                ON wc_conversation_participants(conversation_id, principal_digest)
                WHERE participant_kind = 'human';
            CREATE INDEX IF NOT EXISTS idx_wc_conversation_participants_lookup
                ON wc_conversation_participants(conversation_id, participant_kind, participant_id);

            CREATE TABLE IF NOT EXISTS wc_conversation_messages (
                message_id TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL,
                seq INTEGER NOT NULL CHECK(seq >= 1),
                author_participant_id TEXT NOT NULL,
                body TEXT NOT NULL,
                reply_to_message_id TEXT,
                created_at_unix_ms INTEGER NOT NULL,
                UNIQUE(conversation_id, seq),
                UNIQUE(conversation_id, message_id),
                FOREIGN KEY(conversation_id) REFERENCES wc_conversations(conversation_id),
                FOREIGN KEY(author_participant_id) REFERENCES wc_conversation_participants(participant_id),
                FOREIGN KEY(conversation_id, reply_to_message_id)
                    REFERENCES wc_conversation_messages(conversation_id, message_id)
            );
            CREATE INDEX IF NOT EXISTS idx_wc_conversation_messages_order
                ON wc_conversation_messages(conversation_id, seq);

            CREATE TABLE IF NOT EXISTS wc_agent_deliveries (
                delivery_order INTEGER PRIMARY KEY AUTOINCREMENT,
                delivery_id TEXT NOT NULL UNIQUE,
                message_id TEXT NOT NULL,
                conversation_id TEXT NOT NULL,
                message_seq INTEGER NOT NULL CHECK(message_seq >= 1),
                recipient_agent_id TEXT NOT NULL,
                state TEXT NOT NULL CHECK(state IN ('queued', 'consumed')),
                created_at_unix_ms INTEGER NOT NULL,
                consumed_at_unix_ms INTEGER,
                consumed_by_endpoint_id TEXT,
                UNIQUE(message_id, recipient_agent_id),
                FOREIGN KEY(conversation_id, message_id)
                    REFERENCES wc_conversation_messages(conversation_id, message_id),
                FOREIGN KEY(recipient_agent_id) REFERENCES wc_agent_identities(agent_id),
                FOREIGN KEY(consumed_by_endpoint_id) REFERENCES wc_agent_endpoints(endpoint_id)
            );
            CREATE INDEX IF NOT EXISTS idx_wc_agent_deliveries_inbox
                ON wc_agent_deliveries(recipient_agent_id, state, delivery_order);
            CREATE INDEX IF NOT EXISTS idx_wc_agent_deliveries_message
                ON wc_agent_deliveries(message_id, recipient_agent_id);

            CREATE TABLE IF NOT EXISTS wc_communication_idempotency (
                principal_digest TEXT NOT NULL,
                operation TEXT NOT NULL,
                key_hash TEXT NOT NULL,
                request_hash TEXT NOT NULL,
                resource_id TEXT NOT NULL,
                created_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY(principal_digest, operation, key_hash)
            );
            CREATE INDEX IF NOT EXISTS idx_wc_communication_idempotency_created
                ON wc_communication_idempotency(created_at_unix_ms DESC);
            ",
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn create_agent_identity(
        &self,
        principal: &CommunicationPrincipal,
        input: NewAgentIdentity,
    ) -> Result<AgentIdentityMutation, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        let handle = validate_handle(&input.handle)?;
        let display_name = validate_display_name(&input.display_name)?;
        let description = validate_description(&input.description)?;
        let specialty_labels = canonicalize_specialty_labels(input.specialty_labels)?;
        let idempotency_key = validate_idempotency_key(&input.idempotency_key)?;
        let request_hash = digest_json(&json!({
            "handle": handle,
            "display_name": display_name,
            "description": description,
            "specialty_labels": specialty_labels,
        }));
        let now = now_unix_ms();
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        if let Some(agent_id) = lookup_idempotent_resource(
            &transaction,
            principal,
            OP_CREATE_AGENT,
            &idempotency_key,
            &request_hash,
        )? {
            let agent = load_agent(&transaction, &agent_id)?.ok_or_else(|| {
                CommunicationStoreError::new("agent_not_found", "Agent no longer exists")
            })?;
            return Ok(AgentIdentityMutation {
                agent,
                created: false,
                replayed: true,
                state_changed: false,
            });
        }
        let count: i64 = transaction
            .query_row("SELECT COUNT(*) FROM wc_agent_identities", [], |row| {
                row.get(0)
            })
            .map_err(store_error)?;
        if count >= MAX_DURABLE_AGENTS {
            return Err(CommunicationStoreError::new(
                "agent_capacity_exceeded",
                "Durable Agent capacity is exhausted",
            ));
        }
        let agent_id = new_id(DURABLE_AGENT_ID_PREFIX);
        transaction
            .execute(
                "INSERT INTO wc_agent_identities (
                    agent_id, owner_principal_kind, owner_principal_digest,
                    handle, display_name, description, specialty_labels_json,
                    profile_revision, created_at_unix_ms, updated_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?8)",
                params![
                    agent_id,
                    principal.kind,
                    principal.digest,
                    handle,
                    display_name,
                    description,
                    serde_json::to_string(&specialty_labels).expect("labels serialize"),
                    now,
                ],
            )
            .map_err(store_error)?;
        record_idempotent_resource(
            &transaction,
            principal,
            OP_CREATE_AGENT,
            &idempotency_key,
            &request_hash,
            &agent_id,
            now,
        )?;
        let agent = load_agent(&transaction, &agent_id)?
            .expect("inserted Agent must be readable in the same transaction");
        transaction.commit().map_err(store_error)?;
        Ok(AgentIdentityMutation {
            agent,
            created: true,
            replayed: false,
            state_changed: true,
        })
    }

    pub fn list_agent_identities(
        &self,
        principal: &CommunicationPrincipal,
        agent_id: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<AgentIdentityPage, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        let limit = bounded_limit(limit)?;
        let conn = self.conn.lock().unwrap();
        if let Some(agent_id) = agent_id {
            validate_id(agent_id, DURABLE_AGENT_ID_PREFIX, "invalid_agent_id")?;
            let owned: bool = conn
                .query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM wc_agent_identities
                        WHERE agent_id = ?1 AND owner_principal_kind = ?2
                          AND owner_principal_digest = ?3
                     )",
                    params![agent_id, principal.kind, principal.digest],
                    |row| row.get(0),
                )
                .map_err(store_error)?;
            let agents = if owned {
                load_agent(&conn, agent_id)?.into_iter().collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            return Ok(AgentIdentityPage {
                total_count: agents.len() as i64,
                offset: 0,
                next_offset: None,
                truncated: false,
                agents,
            });
        }
        let total_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM wc_agent_identities
                 WHERE owner_principal_kind = ?1 AND owner_principal_digest = ?2",
                params![principal.kind, principal.digest],
                |row| row.get(0),
            )
            .map_err(store_error)?;
        let mut statement = conn
            .prepare(
                "SELECT agent_id, handle, display_name, description, specialty_labels_json,
                        profile_revision, created_at_unix_ms, updated_at_unix_ms,
                        current_controller_generation,
                        (SELECT COUNT(*) FROM wc_agent_endpoints e
                         WHERE e.agent_id = a.agent_id AND e.lifecycle = 'attached'
                           AND e.controller_generation = a.current_controller_generation
                           AND e.lease_expires_at_unix_ms > ?3),
                        (SELECT COUNT(*) FROM wc_agent_deliveries d
                         WHERE d.recipient_agent_id = a.agent_id AND d.state = 'queued'),
                        (SELECT COUNT(*) FROM wc_agent_wakes w
                         WHERE w.target_agent_id = a.agent_id AND w.state != 'consumed'),
                        (SELECT w.wake_id FROM wc_agent_wakes w
                         WHERE w.target_agent_id = a.agent_id
                         ORDER BY w.created_at_unix_ms DESC, w.wake_id DESC LIMIT 1),
                        (SELECT w.state FROM wc_agent_wakes w
                         WHERE w.target_agent_id = a.agent_id
                         ORDER BY w.created_at_unix_ms DESC, w.wake_id DESC LIMIT 1)
                 FROM wc_agent_identities a
                 WHERE owner_principal_kind = ?1 AND owner_principal_digest = ?2
                 ORDER BY updated_at_unix_ms DESC, agent_id
                 LIMIT ?4 OFFSET ?5",
            )
            .map_err(store_error)?;
        let agents = statement
            .query_map(
                params![
                    principal.kind,
                    principal.digest,
                    now_unix_ms(),
                    limit as i64,
                    offset as i64
                ],
                row_to_agent,
            )
            .map_err(store_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(store_error)?;
        let next_offset = if offset.saturating_add(agents.len()) < total_count as usize {
            Some(offset.saturating_add(agents.len()))
        } else {
            None
        };
        Ok(AgentIdentityPage {
            total_count,
            offset,
            truncated: next_offset.is_some(),
            next_offset,
            agents,
        })
    }

    pub fn update_agent_identity(
        &self,
        principal: &CommunicationPrincipal,
        agent_id: &str,
        expected_profile_revision: i64,
        patch: AgentProfilePatch,
    ) -> Result<AgentIdentityMutation, CommunicationStoreError> {
        validate_id(agent_id, DURABLE_AGENT_ID_PREFIX, "invalid_agent_id")?;
        validate_communication_principal(principal)?;
        if expected_profile_revision < 1 {
            return Err(CommunicationStoreError::new(
                "invalid_profile_revision",
                "expected_profile_revision must be at least 1",
            ));
        }
        if patch.handle.is_none()
            && patch.display_name.is_none()
            && patch.description.is_none()
            && patch.specialty_labels.is_none()
        {
            return Err(CommunicationStoreError::new(
                "invalid_agent_profile_update",
                "At least one Agent profile field must be provided",
            ));
        }
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        require_agent_owner(&transaction, principal, agent_id)?;
        let current = load_agent(&transaction, agent_id)?.expect("owned Agent must exist");
        if current.profile_revision != expected_profile_revision {
            return Err(CommunicationStoreError::profile_changed(
                current.profile_revision,
            ));
        }
        let handle = match patch.handle {
            Some(value) => validate_handle(&value)?,
            None => current.handle.clone(),
        };
        let display_name = match patch.display_name {
            Some(value) => validate_display_name(&value)?,
            None => current.display_name.clone(),
        };
        let description = match patch.description {
            Some(value) => validate_description(&value)?,
            None => current.description.clone(),
        };
        let specialty_labels = match patch.specialty_labels {
            Some(value) => canonicalize_specialty_labels(value)?,
            None => current.specialty_labels.clone(),
        };
        let changed = handle != current.handle
            || display_name != current.display_name
            || description != current.description
            || specialty_labels != current.specialty_labels;
        if !changed {
            return Ok(AgentIdentityMutation {
                agent: current,
                created: false,
                replayed: false,
                state_changed: false,
            });
        }
        let revision = current.profile_revision + 1;
        let now = now_unix_ms().max(current.updated_at_unix_ms.saturating_add(1));
        transaction
            .execute(
                "UPDATE wc_agent_identities
                 SET handle = ?2, display_name = ?3, description = ?4,
                     specialty_labels_json = ?5, profile_revision = ?6,
                     updated_at_unix_ms = ?7
                 WHERE agent_id = ?1 AND profile_revision = ?8",
                params![
                    agent_id,
                    handle,
                    display_name,
                    description,
                    serde_json::to_string(&specialty_labels).expect("labels serialize"),
                    revision,
                    now,
                    expected_profile_revision,
                ],
            )
            .map_err(store_error)?;
        let agent = load_agent(&transaction, agent_id)?
            .expect("updated Agent must remain readable in the same transaction");
        transaction.commit().map_err(store_error)?;
        Ok(AgentIdentityMutation {
            agent,
            created: false,
            replayed: false,
            state_changed: true,
        })
    }

    pub fn attach_agent_endpoint(
        &self,
        principal: &CommunicationPrincipal,
        input: NewAgentEndpoint,
    ) -> Result<AgentEndpointMutation, CommunicationStoreError> {
        validate_id(&input.agent_id, DURABLE_AGENT_ID_PREFIX, "invalid_agent_id")?;
        validate_communication_principal(principal)?;
        let host = validate_nonempty_chars(
            &input.host,
            MAX_ENDPOINT_HOST_CHARS,
            "invalid_endpoint_host",
            "Endpoint host",
        )?;
        let client_attachment_id = validate_optional_chars(
            input.client_attachment_id.as_deref(),
            MAX_ENDPOINT_ATTACHMENT_CHARS,
            "invalid_client_attachment_id",
            "client_attachment_id",
        )?;
        let idempotency_key = validate_idempotency_key(&input.idempotency_key)?;
        let request_hash = digest_json(&json!({
            "agent_id": input.agent_id,
            "host": host,
            "client_attachment_id": client_attachment_id,
            "wake_capable": input.wake_capable,
        }));
        let now = now_unix_ms();
        let lease_expires_at_unix_ms = now.saturating_add(DEFAULT_ENDPOINT_LEASE_MS);
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        if let Some(endpoint_id) = lookup_idempotent_resource(
            &transaction,
            principal,
            OP_ATTACH_ENDPOINT,
            &idempotency_key,
            &request_hash,
        )? {
            let endpoint = load_endpoint(&transaction, &endpoint_id)?.ok_or_else(|| {
                CommunicationStoreError::new(
                    "endpoint_not_found",
                    "Agent Endpoint no longer exists",
                )
            })?;
            return Ok(AgentEndpointMutation {
                endpoint,
                created: false,
                replayed: true,
                state_changed: false,
            });
        }
        require_agent_owner(&transaction, principal, &input.agent_id)?;
        let current_controller_generation: i64 = transaction
            .query_row(
                "SELECT current_controller_generation FROM wc_agent_identities WHERE agent_id = ?1",
                params![input.agent_id],
                |row| row.get(0),
            )
            .map_err(store_error)?;
        let controller_generation =
            current_controller_generation
                .checked_add(1)
                .ok_or_else(|| {
                    CommunicationStoreError::new(
                        "controller_generation_exhausted",
                        "Agent controller generation is exhausted",
                    )
                })?;
        let previous_endpoints = {
            let mut statement = transaction
                .prepare(
                    "SELECT endpoint_id, controller_generation FROM wc_agent_endpoints
                     WHERE agent_id = ?1 AND lifecycle = 'attached'",
                )
                .map_err(store_error)?;
            let endpoints = statement
                .query_map(params![input.agent_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(store_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(store_error)?;
            endpoints
        };
        for (previous_endpoint_id, previous_generation) in &previous_endpoints {
            reconcile_wakes_for_endpoint_loss(
                &transaction,
                &input.agent_id,
                previous_endpoint_id,
                *previous_generation,
                now,
            )?;
        }
        transaction
            .execute(
                "UPDATE wc_agent_endpoints
                 SET lifecycle = 'expired',
                     expired_at_unix_ms = COALESCE(expired_at_unix_ms, ?2),
                     last_seen_at_unix_ms = MAX(last_seen_at_unix_ms, ?2),
                     lease_expires_at_unix_ms = ?2
                 WHERE agent_id = ?1 AND lifecycle = 'attached'",
                params![input.agent_id, now],
            )
            .map_err(store_error)?;
        transaction
            .execute(
                "UPDATE wc_agent_identities
                 SET current_controller_generation = ?2
                 WHERE agent_id = ?1 AND current_controller_generation = ?3",
                params![
                    input.agent_id,
                    controller_generation,
                    current_controller_generation,
                ],
            )
            .map_err(store_error)?;
        let endpoint_id = new_id(AGENT_ENDPOINT_ID_PREFIX);
        transaction
            .execute(
                "INSERT INTO wc_agent_endpoints (
                    endpoint_id, agent_id, attachment_principal_kind,
                    attachment_principal_digest, host, client_attachment_id,
                    wake_capable, controller_generation, lifecycle,
                    attached_at_unix_ms, last_seen_at_unix_ms,
                    lease_expires_at_unix_ms, expired_at_unix_ms,
                    detached_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'attached',
                           ?9, ?9, ?10, NULL, NULL)",
                params![
                    endpoint_id,
                    input.agent_id,
                    principal.kind,
                    principal.digest,
                    host,
                    client_attachment_id,
                    input.wake_capable as i64,
                    controller_generation,
                    now,
                    lease_expires_at_unix_ms,
                ],
            )
            .map_err(store_error)?;
        record_idempotent_resource(
            &transaction,
            principal,
            OP_ATTACH_ENDPOINT,
            &idempotency_key,
            &request_hash,
            &endpoint_id,
            now,
        )?;
        let endpoint = load_endpoint(&transaction, &endpoint_id)?
            .expect("inserted Endpoint must be readable in the same transaction");
        transaction.commit().map_err(store_error)?;
        Ok(AgentEndpointMutation {
            endpoint,
            created: true,
            replayed: false,
            state_changed: true,
        })
    }

    pub fn detach_agent_endpoint(
        &self,
        principal: &CommunicationPrincipal,
        endpoint_id: &str,
    ) -> Result<AgentEndpointMutation, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        validate_id(endpoint_id, AGENT_ENDPOINT_ID_PREFIX, "invalid_endpoint_id")?;
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let current = load_endpoint_for_principal(&transaction, principal, endpoint_id)?
            .ok_or_else(|| {
                CommunicationStoreError::new("endpoint_not_found", "Agent Endpoint does not exist")
            })?;
        if current.lifecycle != AgentEndpointLifecycle::Attached {
            return Ok(AgentEndpointMutation {
                endpoint: current,
                created: false,
                replayed: false,
                state_changed: false,
            });
        }
        require_current_endpoint(
            &transaction,
            principal,
            &current.agent_id,
            endpoint_id,
            Some(current.controller_generation),
        )?;
        let now = now_unix_ms().max(current.last_seen_at_unix_ms);
        reconcile_wakes_for_endpoint_loss(
            &transaction,
            &current.agent_id,
            endpoint_id,
            current.controller_generation,
            now,
        )?;
        transaction
            .execute(
                "UPDATE wc_agent_endpoints
                 SET lifecycle = 'detached', detached_at_unix_ms = ?2,
                     last_seen_at_unix_ms = ?2, lease_expires_at_unix_ms = ?2
                 WHERE endpoint_id = ?1 AND lifecycle = 'attached'",
                params![endpoint_id, now],
            )
            .map_err(store_error)?;
        let endpoint = load_endpoint(&transaction, endpoint_id)?
            .expect("detached Endpoint must remain readable");
        transaction.commit().map_err(store_error)?;
        Ok(AgentEndpointMutation {
            endpoint,
            created: false,
            replayed: false,
            state_changed: true,
        })
    }

    pub fn renew_agent_endpoint(
        &self,
        principal: &CommunicationPrincipal,
        endpoint_id: &str,
        expected_controller_generation: i64,
    ) -> Result<AgentEndpointMutation, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        validate_id(endpoint_id, AGENT_ENDPOINT_ID_PREFIX, "invalid_endpoint_id")?;
        if expected_controller_generation < 1 {
            return Err(CommunicationStoreError::new(
                "invalid_controller_generation",
                "expected_controller_generation must be at least 1",
            ));
        }
        let now = now_unix_ms();
        let lease_expires_at_unix_ms = now.saturating_add(DEFAULT_ENDPOINT_LEASE_MS);
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let current = load_endpoint_for_principal(&transaction, principal, endpoint_id)?
            .ok_or_else(|| {
                CommunicationStoreError::new("endpoint_not_found", "Agent Endpoint does not exist")
            })?;
        require_current_endpoint(
            &transaction,
            principal,
            &current.agent_id,
            endpoint_id,
            Some(expected_controller_generation),
        )?;
        transaction
            .execute(
                "UPDATE wc_agent_endpoints
                 SET last_seen_at_unix_ms = MAX(last_seen_at_unix_ms, ?2),
                     lease_expires_at_unix_ms = MAX(lease_expires_at_unix_ms, ?3)
                 WHERE endpoint_id = ?1 AND lifecycle = 'attached'",
                params![endpoint_id, now, lease_expires_at_unix_ms],
            )
            .map_err(store_error)?;
        let endpoint = load_endpoint(&transaction, endpoint_id)?
            .expect("renewed Endpoint must remain readable");
        transaction.commit().map_err(store_error)?;
        Ok(AgentEndpointMutation {
            endpoint,
            created: false,
            replayed: false,
            state_changed: true,
        })
    }

    /// Verify one exact current Endpoint without exposing process-local Host
    /// adapter state.
    pub fn verify_current_agent_endpoint(
        &self,
        principal: &CommunicationPrincipal,
        agent_id: &str,
        endpoint_id: &str,
        expected_controller_generation: i64,
    ) -> Result<AgentEndpointRecord, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        let conn = self.conn.lock().unwrap();
        require_current_endpoint(
            &conn,
            principal,
            agent_id,
            endpoint_id,
            Some(expected_controller_generation),
        )
    }

    /// Project one current process-local continuation binding onto the durable
    /// Endpoint capability bit. Only Host/controller infrastructure calls this
    /// exact Endpoint-generation transition; public attach requests always
    /// create non-wake-capable Endpoints.
    pub fn set_agent_endpoint_wake_capability(
        &self,
        principal: &CommunicationPrincipal,
        agent_id: &str,
        endpoint_id: &str,
        expected_controller_generation: i64,
        wake_capable: bool,
    ) -> Result<AgentEndpointRecord, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let current = require_current_endpoint(
            &transaction,
            principal,
            agent_id,
            endpoint_id,
            Some(expected_controller_generation),
        )?;
        if current.wake_capable == wake_capable {
            transaction.commit().map_err(store_error)?;
            return Ok(current);
        }
        let now = now_unix_ms();
        if !wake_capable {
            reconcile_wakes_for_endpoint_loss(
                &transaction,
                agent_id,
                endpoint_id,
                expected_controller_generation,
                now,
            )?;
        }
        transaction
            .execute(
                "UPDATE wc_agent_endpoints
                 SET wake_capable = ?2,
                     last_seen_at_unix_ms = MAX(last_seen_at_unix_ms, ?3)
                 WHERE endpoint_id = ?1 AND agent_id = ?4
                   AND controller_generation = ?5 AND lifecycle = 'attached'",
                params![
                    endpoint_id,
                    wake_capable as i64,
                    now,
                    agent_id,
                    expected_controller_generation,
                ],
            )
            .map_err(store_error)?;
        let endpoint = load_endpoint(&transaction, endpoint_id)?
            .expect("current Endpoint must remain readable after capability transition");
        transaction.commit().map_err(store_error)?;
        Ok(endpoint)
    }

    pub fn create_conversation(
        &self,
        principal: &CommunicationPrincipal,
        input: NewConversation,
    ) -> Result<ConversationMutation, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        let title = validate_optional_chars(
            input.title.as_deref(),
            MAX_CONVERSATION_TITLE_CHARS,
            "invalid_conversation_title",
            "Conversation title",
        )?;
        let agent_ids = canonicalize_agent_ids(input.agent_ids, true)?;
        let idempotency_key = validate_idempotency_key(&input.idempotency_key)?;
        let request_hash = digest_json(&json!({"title": title, "agent_ids": agent_ids}));
        let now = now_unix_ms();
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        if let Some(conversation_id) = lookup_idempotent_resource(
            &transaction,
            principal,
            OP_CREATE_CONVERSATION,
            &idempotency_key,
            &request_hash,
        )? {
            let conversation = read_conversation_in_connection(
                &transaction,
                principal,
                &ConversationAccess::Human,
                &conversation_id,
                0,
                1,
            )?;
            return Ok(ConversationMutation {
                conversation,
                created: false,
                replayed: true,
                state_changed: false,
            });
        }
        let conversation_count: i64 = transaction
            .query_row("SELECT COUNT(*) FROM wc_conversations", [], |row| {
                row.get(0)
            })
            .map_err(store_error)?;
        if conversation_count >= MAX_CONVERSATIONS {
            return Err(CommunicationStoreError::new(
                "conversation_capacity_exceeded",
                "Conversation capacity is exhausted",
            ));
        }
        for agent_id in &agent_ids {
            require_agent_owner(&transaction, principal, agent_id)?;
        }
        let conversation_id = new_id(CONVERSATION_ID_PREFIX);
        transaction
            .execute(
                "INSERT INTO wc_conversations (
                    conversation_id, title, lifecycle, next_seq,
                    created_by_principal_kind, created_by_principal_digest,
                    created_at_unix_ms, updated_at_unix_ms, closed_at_unix_ms
                 ) VALUES (?1, ?2, 'open', 1, ?3, ?4, ?5, ?5, NULL)",
                params![
                    conversation_id,
                    title,
                    principal.kind,
                    principal.digest,
                    now
                ],
            )
            .map_err(store_error)?;
        transaction
            .execute(
                "INSERT INTO wc_conversation_participants (
                    participant_id, conversation_id, participant_kind, agent_id,
                    principal_kind, principal_digest, joined_at_unix_ms
                 ) VALUES (?1, ?2, 'human', NULL, ?3, ?4, ?5)",
                params![
                    new_id(CONVERSATION_PARTICIPANT_ID_PREFIX),
                    conversation_id,
                    principal.kind,
                    principal.digest,
                    now,
                ],
            )
            .map_err(store_error)?;
        for agent_id in &agent_ids {
            transaction
                .execute(
                    "INSERT INTO wc_conversation_participants (
                        participant_id, conversation_id, participant_kind, agent_id,
                        principal_kind, principal_digest, joined_at_unix_ms
                     ) VALUES (?1, ?2, 'agent', ?3, NULL, NULL, ?4)",
                    params![
                        new_id(CONVERSATION_PARTICIPANT_ID_PREFIX),
                        conversation_id,
                        agent_id,
                        now,
                    ],
                )
                .map_err(store_error)?;
        }
        record_idempotent_resource(
            &transaction,
            principal,
            OP_CREATE_CONVERSATION,
            &idempotency_key,
            &request_hash,
            &conversation_id,
            now,
        )?;
        let conversation = read_conversation_in_connection(
            &transaction,
            principal,
            &ConversationAccess::Human,
            &conversation_id,
            0,
            1,
        )?;
        transaction.commit().map_err(store_error)?;
        Ok(ConversationMutation {
            conversation,
            created: true,
            replayed: false,
            state_changed: true,
        })
    }

    pub fn list_conversations(
        &self,
        principal: &CommunicationPrincipal,
        access: &ConversationAccess,
        offset: usize,
        limit: usize,
    ) -> Result<ConversationPage, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        let limit = bounded_limit(limit)?;
        let conn = self.conn.lock().unwrap();
        let agent_id = authorize_list_access(&conn, principal, access)?;
        let (where_clause, identity) = match agent_id.as_deref() {
            Some(agent_id) => (
                "EXISTS (
                    SELECT 1 FROM wc_conversation_participants p
                    WHERE p.conversation_id = c.conversation_id
                      AND p.participant_kind = 'agent' AND p.agent_id = ?1
                 )",
                agent_id,
            ),
            None => (
                "EXISTS (
                    SELECT 1 FROM wc_conversation_participants p
                    WHERE p.conversation_id = c.conversation_id
                      AND p.participant_kind = 'human' AND p.principal_digest = ?1
                 )",
                principal.digest.as_str(),
            ),
        };
        let count_sql = format!("SELECT COUNT(*) FROM wc_conversations c WHERE {where_clause}");
        let total_count: i64 = conn
            .query_row(&count_sql, params![identity], |row| row.get(0))
            .map_err(store_error)?;
        let queued_expression = if agent_id.is_some() {
            "(SELECT COUNT(*) FROM wc_agent_deliveries d
               WHERE d.conversation_id = c.conversation_id
                 AND d.recipient_agent_id = ?1 AND d.state = 'queued')"
        } else {
            "NULL"
        };
        let sql = format!(
            "SELECT c.conversation_id, c.title, c.lifecycle,
                    c.created_at_unix_ms, c.updated_at_unix_ms,
                    (SELECT COUNT(*) FROM wc_conversation_participants p2
                     WHERE p2.conversation_id = c.conversation_id),
                    c.next_seq - 1,
                    c.next_seq - 1,
                    {queued_expression}
             FROM wc_conversations c
             WHERE {where_clause}
             ORDER BY c.updated_at_unix_ms DESC, c.conversation_id
             LIMIT ?2 OFFSET ?3"
        );
        let mut statement = conn.prepare(&sql).map_err(store_error)?;
        let conversations = statement
            .query_map(params![identity, limit as i64, offset as i64], |row| {
                row_to_conversation_summary(row)
            })
            .map_err(store_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(store_error)?;
        let next_offset = if offset.saturating_add(conversations.len()) < total_count as usize {
            Some(offset.saturating_add(conversations.len()))
        } else {
            None
        };
        Ok(ConversationPage {
            total_count,
            offset,
            truncated: next_offset.is_some(),
            next_offset,
            conversations,
        })
    }

    pub fn read_conversation(
        &self,
        principal: &CommunicationPrincipal,
        access: &ConversationAccess,
        conversation_id: &str,
        after_seq: i64,
        limit: usize,
    ) -> Result<ConversationDetailRecord, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        validate_id(
            conversation_id,
            CONVERSATION_ID_PREFIX,
            "invalid_conversation_id",
        )?;
        if after_seq < 0 {
            return Err(CommunicationStoreError::new(
                "invalid_conversation_sequence",
                "after_seq must be zero or greater",
            ));
        }
        let limit = bounded_limit(limit)?;
        let conn = self.conn.lock().unwrap();
        read_conversation_in_connection(&conn, principal, access, conversation_id, after_seq, limit)
    }

    pub fn post_conversation_message(
        &self,
        principal: &CommunicationPrincipal,
        input: NewConversationMessage,
    ) -> Result<ConversationMessageMutation, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        validate_id(
            &input.conversation_id,
            CONVERSATION_ID_PREFIX,
            "invalid_conversation_id",
        )?;
        let body = validate_message_body(&input.body)?;
        let (operation, idempotency_key, wake_reply) = match (
            input.idempotency_key.as_deref(),
            input.wake_reply_id.as_deref(),
            input.reply_operation_index,
        ) {
            (Some(key), None, None) => (OP_POST_MESSAGE, validate_idempotency_key(key)?, None),
            (None, Some(wake_id), Some(operation_index)) => {
                validate_id(
                    wake_id,
                    super::agent_wake::AGENT_WAKE_ID_PREFIX,
                    "invalid_wake_id",
                )?;
                if !(0..=MAX_WAKE_REPLY_OPERATION_INDEX).contains(&operation_index) {
                    return Err(CommunicationStoreError::new(
                        "invalid_reply_operation_index",
                        format!(
                            "reply_operation_index must be between 0 and {MAX_WAKE_REPLY_OPERATION_INDEX}"
                        ),
                    ));
                }
                (
                    OP_POST_WAKE_REPLY,
                    format!("{wake_id}:{operation_index}"),
                    Some((wake_id.to_string(), operation_index)),
                )
            }
            (Some(_), Some(_), Some(_)) => {
                return Err(CommunicationStoreError::new(
                    "conflicting_message_replay_identity",
                    "Provide idempotency_key or wake_reply_id plus reply_operation_index, not both",
                ));
            }
            _ => {
                return Err(CommunicationStoreError::new(
                    "message_replay_identity_required",
                    "Provide idempotency_key, or wake_reply_id plus reply_operation_index",
                ));
            }
        };
        let reply_to = match input.reply_to {
            Some(value) => {
                validate_id(&value, CONVERSATION_MESSAGE_ID_PREFIX, "invalid_message_id")?;
                Some(value)
            }
            None => None,
        };
        let explicit_recipient_agent_ids = match input.recipient_agent_ids.as_ref() {
            Some(agent_ids) => Some(canonicalize_agent_ids(agent_ids.clone(), false)?),
            None => None,
        };
        let access = match input.author_agent_id.as_ref() {
            Some(agent_id) => {
                let endpoint_id = input.endpoint_id.as_ref().ok_or_else(|| {
                    CommunicationStoreError::new(
                        "endpoint_required",
                        "Agent-authored messages require an active Endpoint",
                    )
                })?;
                let expected_controller_generation =
                    input.expected_controller_generation.ok_or_else(|| {
                        CommunicationStoreError::new(
                            "controller_generation_required",
                            "Agent-authored messages require expected_controller_generation",
                        )
                    })?;
                ConversationAccess::Agent {
                    agent_id: agent_id.clone(),
                    endpoint_id: endpoint_id.clone(),
                    expected_controller_generation,
                }
            }
            None => {
                if input.endpoint_id.is_some() || input.expected_controller_generation.is_some() {
                    return Err(CommunicationStoreError::new(
                        "unexpected_endpoint_id",
                        "Human-authored messages must not provide Endpoint fencing",
                    ));
                }
                if wake_reply.is_some() {
                    return Err(CommunicationStoreError::new(
                        "wake_reply_requires_agent_author",
                        "Wake-derived reply replay identity requires an Agent author",
                    ));
                }
                ConversationAccess::Human
            }
        };
        // Hash the caller's canonical logical request rather than derived current
        // state. Exact replay can therefore recover the committed Message even
        // after its author Endpoint detached or the Conversation later closed.
        // Wake-derived reply replay is stable across Host/Endpoint replacement.
        // The exact current carrier is still required for the first commit, but
        // is not part of the semantic send identity recovered after an
        // uncertain response.
        let replay_endpoint_id = wake_reply
            .is_none()
            .then(|| input.endpoint_id.as_deref())
            .flatten();
        let replay_controller_generation = wake_reply
            .is_none()
            .then_some(input.expected_controller_generation)
            .flatten();
        let request_hash = digest_json(&json!({
            "conversation_id": &input.conversation_id,
            "author_agent_id": input.author_agent_id.as_deref(),
            "endpoint_id": replay_endpoint_id,
            "expected_controller_generation": replay_controller_generation,
            "body": &body,
            "recipient_agent_ids": explicit_recipient_agent_ids.as_ref(),
            "reply_to": reply_to.as_deref(),
            "wake_reply_id": wake_reply.as_ref().map(|(wake_id, _)| wake_id),
            "reply_operation_index": wake_reply.as_ref().map(|(_, index)| index),
        }));
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        if let Some(message_id) = lookup_idempotent_resource(
            &transaction,
            principal,
            operation,
            &idempotency_key,
            &request_hash,
        )? {
            let message = load_message(&transaction, &message_id)?.ok_or_else(|| {
                CommunicationStoreError::new(
                    "message_not_found",
                    "Conversation Message no longer exists",
                )
            })?;
            return Ok(ConversationMessageMutation {
                message,
                replayed: true,
                state_changed: false,
            });
        }
        authorize_conversation_access(&transaction, principal, &access, &input.conversation_id)?;
        if let Some((wake_id, _)) = wake_reply.as_ref() {
            let ConversationAccess::Agent {
                agent_id,
                endpoint_id,
                expected_controller_generation,
            } = &access
            else {
                unreachable!("Wake reply identity requires Agent access");
            };
            let wake_binding: Option<(AgentWakeState, Option<String>, Option<i64>)> = transaction
                .query_row(
                    "SELECT state, claimed_endpoint_id, claimed_controller_generation
                     FROM wc_agent_wakes
                     WHERE wake_id = ?1 AND target_agent_id = ?2",
                    params![wake_id, agent_id],
                    |row| {
                        let state = row.get::<_, String>(0)?;
                        Ok((
                            AgentWakeState::from_db(&state, 0)?,
                            row.get(1)?,
                            row.get(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(store_error)?;
            let Some((wake_state, claimed_endpoint_id, claimed_generation)) = wake_binding else {
                return Err(CommunicationStoreError::new(
                    "wake_not_found",
                    "Agent Wake does not exist",
                ));
            };
            match wake_state {
                AgentWakeState::Prepared
                | AgentWakeState::Delivered
                | AgentWakeState::DeliveryUnknown => {
                    if claimed_endpoint_id.as_deref() != Some(endpoint_id.as_str())
                        || claimed_generation != Some(*expected_controller_generation)
                    {
                        return Err(CommunicationStoreError::new(
                            "wake_endpoint_fence_mismatch",
                            "Agent Wake is bound to a different Endpoint generation",
                        ));
                    }
                }
                AgentWakeState::Pending | AgentWakeState::Claimed => {
                    return Err(CommunicationStoreError::new(
                        "wake_not_dispatched",
                        "Agent Wake reply requires a dispatch or explicit-activation fence",
                    ));
                }
                AgentWakeState::Consumed => {
                    return Err(CommunicationStoreError::new(
                        "wake_already_consumed",
                        "Agent Wake was already consumed; re-read the Conversation before posting new work",
                    ));
                }
            }
        }
        let lifecycle = transaction
            .query_row(
                "SELECT lifecycle FROM wc_conversations WHERE conversation_id = ?1",
                params![input.conversation_id],
                |row| {
                    let lifecycle = row.get::<_, String>(0)?;
                    ConversationLifecycle::from_db(&lifecycle, 0)
                },
            )
            .map_err(store_error)?;
        if lifecycle != ConversationLifecycle::Open {
            return Err(CommunicationStoreError::new(
                "conversation_closed",
                "Conversation is closed",
            ));
        }
        let (author_participant_id, author_agent_id) = match &access {
            ConversationAccess::Agent { agent_id, .. } => {
                let participant_id: String = transaction
                    .query_row(
                        "SELECT participant_id FROM wc_conversation_participants
                         WHERE conversation_id = ?1 AND participant_kind = 'agent' AND agent_id = ?2",
                        params![input.conversation_id, agent_id],
                        |row| row.get(0),
                    )
                    .map_err(store_error)?;
                (participant_id, Some(agent_id.clone()))
            }
            ConversationAccess::Human => {
                let participant_id: String = transaction
                    .query_row(
                        "SELECT participant_id FROM wc_conversation_participants
                         WHERE conversation_id = ?1 AND participant_kind = 'human'
                           AND principal_kind = ?2 AND principal_digest = ?3",
                        params![input.conversation_id, principal.kind, principal.digest],
                        |row| row.get(0),
                    )
                    .map_err(store_error)?;
                (participant_id, None)
            }
        };
        if let Some(reply_to) = reply_to.as_deref() {
            let reply_exists: bool = transaction
                .query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM wc_conversation_messages
                        WHERE message_id = ?1 AND conversation_id = ?2
                     )",
                    params![reply_to, input.conversation_id],
                    |row| row.get(0),
                )
                .map_err(store_error)?;
            if !reply_exists {
                return Err(CommunicationStoreError::new(
                    "reply_message_not_found",
                    "reply_to message does not exist in this Conversation",
                ));
            }
        }
        let recipient_agent_ids = match explicit_recipient_agent_ids {
            Some(agent_ids) => agent_ids,
            None => {
                let mut statement = transaction
                    .prepare(
                        "SELECT agent_id FROM wc_conversation_participants
                         WHERE conversation_id = ?1 AND participant_kind = 'agent'
                         ORDER BY agent_id",
                    )
                    .map_err(store_error)?;
                let agent_ids = statement
                    .query_map(params![input.conversation_id], |row| {
                        row.get::<_, String>(0)
                    })
                    .map_err(store_error)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(store_error)?;
                agent_ids
                    .into_iter()
                    .filter(|candidate| author_agent_id.as_deref() != Some(candidate.as_str()))
                    .collect()
            }
        };
        for recipient_agent_id in &recipient_agent_ids {
            if author_agent_id.as_deref() == Some(recipient_agent_id.as_str()) {
                return Err(CommunicationStoreError::new(
                    "self_delivery_not_supported",
                    "Agent-authored messages cannot create an Inbox delivery for the author",
                ));
            }
            let participant: bool = transaction
                .query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM wc_conversation_participants
                        WHERE conversation_id = ?1 AND participant_kind = 'agent' AND agent_id = ?2
                     )",
                    params![input.conversation_id, recipient_agent_id],
                    |row| row.get(0),
                )
                .map_err(store_error)?;
            if !participant {
                return Err(CommunicationStoreError::new(
                    "recipient_not_conversation_participant",
                    format!(
                        "Recipient Agent is not a Conversation participant: {recipient_agent_id}"
                    ),
                ));
            }
        }
        let next_seq: i64 = transaction
            .query_row(
                "SELECT next_seq FROM wc_conversations WHERE conversation_id = ?1",
                params![input.conversation_id],
                |row| row.get(0),
            )
            .map_err(store_error)?;
        if next_seq > MAX_MESSAGES_PER_CONVERSATION {
            return Err(CommunicationStoreError::new(
                "conversation_message_capacity_exceeded",
                "Conversation has reached the message capacity bound",
            ));
        }
        let now = now_unix_ms();
        let message_id = new_id(CONVERSATION_MESSAGE_ID_PREFIX);
        transaction
            .execute(
                "INSERT INTO wc_conversation_messages (
                    message_id, conversation_id, seq, author_participant_id,
                    body, reply_to_message_id, created_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    message_id,
                    input.conversation_id,
                    next_seq,
                    author_participant_id,
                    body,
                    reply_to,
                    now,
                ],
            )
            .map_err(store_error)?;
        for recipient_agent_id in &recipient_agent_ids {
            let delivery_id = new_id(AGENT_DELIVERY_ID_PREFIX);
            transaction
                .execute(
                    "INSERT INTO wc_agent_deliveries (
                        delivery_id, message_id, conversation_id, message_seq,
                        recipient_agent_id, state, created_at_unix_ms,
                        consumed_at_unix_ms, consumed_by_endpoint_id
                     ) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, NULL, NULL)",
                    params![
                        delivery_id,
                        message_id,
                        input.conversation_id,
                        next_seq,
                        recipient_agent_id,
                        now,
                    ],
                )
                .map_err(store_error)?;
            let delivery_order = transaction.last_insert_rowid();
            coalesce_agent_wake_for_delivery(
                &transaction,
                recipient_agent_id,
                &delivery_id,
                &input.conversation_id,
                &message_id,
                delivery_order,
                now,
            )?;
        }
        transaction
            .execute(
                "UPDATE wc_conversations
                 SET next_seq = next_seq + 1, updated_at_unix_ms = ?2
                 WHERE conversation_id = ?1",
                params![input.conversation_id, now],
            )
            .map_err(store_error)?;
        if let (Some(agent_id), Some(endpoint_id)) =
            (author_agent_id.as_deref(), input.endpoint_id.as_deref())
        {
            transaction
                .execute(
                    "UPDATE wc_agent_endpoints SET last_seen_at_unix_ms = MAX(last_seen_at_unix_ms, ?3)
                     WHERE endpoint_id = ?1 AND agent_id = ?2 AND lifecycle = 'attached'",
                    params![endpoint_id, agent_id, now],
                )
                .map_err(store_error)?;
        }
        record_idempotent_resource(
            &transaction,
            principal,
            operation,
            &idempotency_key,
            &request_hash,
            &message_id,
            now,
        )?;
        let message = load_message(&transaction, &message_id)?
            .expect("inserted Message must be readable in the same transaction");
        transaction.commit().map_err(store_error)?;
        Ok(ConversationMessageMutation {
            message,
            replayed: false,
            state_changed: true,
        })
    }

    pub fn list_agent_inbox(
        &self,
        principal: &CommunicationPrincipal,
        agent_id: &str,
        endpoint_id: &str,
        expected_controller_generation: i64,
        after_delivery_order: i64,
        limit: usize,
    ) -> Result<AgentInboxPage, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        if after_delivery_order < 0 {
            return Err(CommunicationStoreError::new(
                "invalid_delivery_order",
                "after_delivery_order must be zero or greater",
            ));
        }
        let limit = bounded_limit(limit)?;
        let conn = self.conn.lock().unwrap();
        require_current_endpoint(
            &conn,
            principal,
            agent_id,
            endpoint_id,
            Some(expected_controller_generation),
        )?;
        let total_queued_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM wc_agent_deliveries
                 WHERE recipient_agent_id = ?1 AND state = 'queued'",
                params![agent_id],
                |row| row.get(0),
            )
            .map_err(store_error)?;
        let mut statement = conn
            .prepare(
                "SELECT delivery_order, delivery_id, message_id, conversation_id,
                        (SELECT title FROM wc_conversations c
                         WHERE c.conversation_id = d.conversation_id)
                 FROM wc_agent_deliveries d
                 WHERE recipient_agent_id = ?1 AND state = 'queued' AND delivery_order > ?2
                 ORDER BY delivery_order
                 LIMIT ?3",
            )
            .map_err(store_error)?;
        let rows = statement
            .query_map(
                params![agent_id, after_delivery_order, (limit + 1) as i64],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .map_err(store_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(store_error)?;
        let truncated = rows.len() > limit;
        let mut deliveries = Vec::with_capacity(rows.len().min(limit));
        let mut delivery_orders = Vec::with_capacity(rows.len().min(limit));
        for (delivery_order, delivery_id, message_id, conversation_id, conversation_title) in
            rows.into_iter().take(limit)
        {
            delivery_orders.push(delivery_order);
            let message = load_message(&conn, &message_id)?.ok_or_else(|| {
                CommunicationStoreError::new("message_not_found", "Inbox Message no longer exists")
            })?;
            deliveries.push(AgentInboxItem {
                delivery_id,
                state: MessageDeliveryState::Queued,
                conversation_id,
                conversation_title,
                message,
            });
        }
        let next_after_delivery_order = if truncated {
            delivery_orders.last().copied()
        } else {
            None
        };
        Ok(AgentInboxPage {
            agent_id: agent_id.to_string(),
            total_queued_count,
            after_delivery_order,
            next_after_delivery_order,
            truncated,
            deliveries,
        })
    }

    pub fn consume_agent_deliveries(
        &self,
        principal: &CommunicationPrincipal,
        agent_id: &str,
        endpoint_id: &str,
        expected_controller_generation: i64,
        delivery_ids: Vec<String>,
    ) -> Result<DeliveryConsumeResult, CommunicationStoreError> {
        validate_communication_principal(principal)?;
        validate_id(agent_id, DURABLE_AGENT_ID_PREFIX, "invalid_agent_id")?;
        validate_id(endpoint_id, AGENT_ENDPOINT_ID_PREFIX, "invalid_endpoint_id")?;
        let delivery_ids = canonicalize_delivery_ids(delivery_ids)?;
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        require_current_endpoint(
            &transaction,
            principal,
            agent_id,
            endpoint_id,
            Some(expected_controller_generation),
        )?;
        let now = now_unix_ms();
        let mut consumed_delivery_ids = Vec::new();
        let mut already_consumed_delivery_ids = Vec::new();
        for delivery_id in &delivery_ids {
            let state = transaction
                .query_row(
                    "SELECT state FROM wc_agent_deliveries
                     WHERE delivery_id = ?1 AND recipient_agent_id = ?2",
                    params![delivery_id, agent_id],
                    |row| {
                        let state = row.get::<_, String>(0)?;
                        MessageDeliveryState::from_db(&state, 0)
                    },
                )
                .optional()
                .map_err(store_error)?;
            let Some(state) = state else {
                return Err(CommunicationStoreError::new(
                    "delivery_not_found",
                    "Agent delivery does not exist",
                ));
            };
            if state == MessageDeliveryState::Consumed {
                already_consumed_delivery_ids.push(delivery_id.clone());
                continue;
            }
            transaction
                .execute(
                    "UPDATE wc_agent_deliveries
                     SET state = 'consumed', consumed_at_unix_ms = ?3,
                         consumed_by_endpoint_id = ?4
                     WHERE delivery_id = ?1 AND recipient_agent_id = ?2 AND state = 'queued'",
                    params![delivery_id, agent_id, now, endpoint_id],
                )
                .map_err(store_error)?;
            consumed_delivery_ids.push(delivery_id.clone());
        }
        transaction
            .execute(
                "UPDATE wc_agent_endpoints SET last_seen_at_unix_ms = MAX(last_seen_at_unix_ms, ?2)
                 WHERE endpoint_id = ?1 AND lifecycle = 'attached'",
                params![endpoint_id, now],
            )
            .map_err(store_error)?;
        let state_changed = !consumed_delivery_ids.is_empty();
        transaction.commit().map_err(store_error)?;
        Ok(DeliveryConsumeResult {
            agent_id: agent_id.to_string(),
            consumed_delivery_ids,
            already_consumed_delivery_ids,
            state_changed,
        })
    }
}

fn authorize_list_access(
    conn: &Connection,
    principal: &CommunicationPrincipal,
    access: &ConversationAccess,
) -> Result<Option<String>, CommunicationStoreError> {
    match access {
        ConversationAccess::Human => Ok(None),
        ConversationAccess::Agent {
            agent_id,
            endpoint_id,
            expected_controller_generation,
        } => {
            require_current_endpoint(
                conn,
                principal,
                agent_id,
                endpoint_id,
                Some(*expected_controller_generation),
            )?;
            Ok(Some(agent_id.clone()))
        }
    }
}

pub(super) fn authorize_conversation_access(
    conn: &Connection,
    principal: &CommunicationPrincipal,
    access: &ConversationAccess,
    conversation_id: &str,
) -> Result<Option<String>, CommunicationStoreError> {
    let participant: bool = match access {
        ConversationAccess::Human => conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM wc_conversation_participants
                    WHERE conversation_id = ?1 AND participant_kind = 'human'
                      AND principal_kind = ?2 AND principal_digest = ?3
                 )",
                params![conversation_id, principal.kind, principal.digest],
                |row| row.get(0),
            )
            .map_err(store_error)?,
        ConversationAccess::Agent {
            agent_id,
            endpoint_id,
            expected_controller_generation,
        } => {
            require_current_endpoint(
                conn,
                principal,
                agent_id,
                endpoint_id,
                Some(*expected_controller_generation),
            )?;
            conn.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM wc_conversation_participants
                    WHERE conversation_id = ?1 AND participant_kind = 'agent' AND agent_id = ?2
                 )",
                params![conversation_id, agent_id],
                |row| row.get(0),
            )
            .map_err(store_error)?
        }
    };
    if !participant {
        return Err(CommunicationStoreError::new(
            "conversation_not_found",
            "Conversation does not exist",
        ));
    }
    match access {
        ConversationAccess::Human => Ok(None),
        ConversationAccess::Agent { agent_id, .. } => Ok(Some(agent_id.clone())),
    }
}

pub(super) fn read_conversation_in_connection(
    conn: &Connection,
    principal: &CommunicationPrincipal,
    access: &ConversationAccess,
    conversation_id: &str,
    after_seq: i64,
    limit: usize,
) -> Result<ConversationDetailRecord, CommunicationStoreError> {
    let access_agent_id = authorize_conversation_access(conn, principal, access, conversation_id)?;
    let queued_expression = "CASE WHEN ?2 IS NULL THEN NULL ELSE
        (SELECT COUNT(*) FROM wc_agent_deliveries d
         WHERE d.conversation_id = c.conversation_id
           AND d.recipient_agent_id = ?2 AND d.state = 'queued') END";
    let summary_sql = format!(
        "SELECT c.conversation_id, c.title, c.lifecycle,
                c.created_at_unix_ms, c.updated_at_unix_ms,
                (SELECT COUNT(*) FROM wc_conversation_participants p
                 WHERE p.conversation_id = c.conversation_id),
                c.next_seq - 1,
                c.next_seq - 1,
                {queued_expression}
         FROM wc_conversations c WHERE c.conversation_id = ?1"
    );
    let summary = conn
        .query_row(
            &summary_sql,
            params![conversation_id, access_agent_id.as_deref()],
            row_to_conversation_summary,
        )
        .map_err(store_error)?;
    let mut participant_statement = conn
        .prepare(
            "SELECT p.participant_id, p.participant_kind, p.agent_id,
                    a.handle, a.display_name, p.principal_kind, p.joined_at_unix_ms
             FROM wc_conversation_participants p
             LEFT JOIN wc_agent_identities a ON a.agent_id = p.agent_id
             WHERE p.conversation_id = ?1
             ORDER BY CASE p.participant_kind WHEN 'human' THEN 0 ELSE 1 END,
                      COALESCE(a.handle, p.principal_kind), p.participant_id",
        )
        .map_err(store_error)?;
    let participants = participant_statement
        .query_map(params![conversation_id], |row| {
            Ok(ConversationParticipantRecord {
                participant_id: row.get(0)?,
                participant_kind: row.get(1)?,
                agent_id: row.get(2)?,
                handle: row.get(3)?,
                display_name: row.get(4)?,
                principal_kind: row.get(5)?,
                joined_at_unix_ms: row.get(6)?,
            })
        })
        .map_err(store_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(store_error)?;
    let mut message_statement = conn
        .prepare(
            "SELECT message_id FROM wc_conversation_messages
             WHERE conversation_id = ?1 AND seq > ?2
             ORDER BY seq
             LIMIT ?3",
        )
        .map_err(store_error)?;
    let message_ids = message_statement
        .query_map(
            params![conversation_id, after_seq, (limit + 1) as i64],
            |row| row.get::<_, String>(0),
        )
        .map_err(store_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(store_error)?;
    let truncated = message_ids.len() > limit;
    let mut messages = Vec::with_capacity(message_ids.len().min(limit));
    for message_id in message_ids.into_iter().take(limit) {
        messages.push(load_message(conn, &message_id)?.ok_or_else(|| {
            CommunicationStoreError::new("message_not_found", "Conversation Message disappeared")
        })?);
    }
    let next_after_seq = if truncated {
        messages.last().map(|message| message.seq)
    } else {
        None
    };
    Ok(ConversationDetailRecord {
        conversation: summary,
        participants,
        messages,
        after_seq,
        next_after_seq,
        truncated,
    })
}

fn require_agent_owner(
    conn: &Connection,
    principal: &CommunicationPrincipal,
    agent_id: &str,
) -> Result<(), CommunicationStoreError> {
    validate_id(agent_id, DURABLE_AGENT_ID_PREFIX, "invalid_agent_id")?;
    let owned: bool = conn
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM wc_agent_identities
                WHERE agent_id = ?1
                  AND owner_principal_kind = ?2
                  AND owner_principal_digest = ?3
             )",
            params![agent_id, principal.kind, principal.digest],
            |row| row.get(0),
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

pub(super) fn require_current_endpoint(
    conn: &Connection,
    principal: &CommunicationPrincipal,
    agent_id: &str,
    endpoint_id: &str,
    expected_controller_generation: Option<i64>,
) -> Result<AgentEndpointRecord, CommunicationStoreError> {
    validate_id(agent_id, DURABLE_AGENT_ID_PREFIX, "invalid_agent_id")?;
    validate_id(endpoint_id, AGENT_ENDPOINT_ID_PREFIX, "invalid_endpoint_id")?;
    if expected_controller_generation.is_some_and(|generation| generation < 1) {
        return Err(CommunicationStoreError::new(
            "invalid_controller_generation",
            "expected_controller_generation must be at least 1",
        ));
    }
    let row = load_endpoint_for_principal(conn, principal, endpoint_id)?.ok_or_else(|| {
        CommunicationStoreError::new("endpoint_not_found", "Agent Endpoint does not exist")
    })?;
    if row.agent_id != agent_id {
        return Err(CommunicationStoreError::new(
            "endpoint_agent_mismatch",
            "Agent Endpoint is attached to a different Agent",
        ));
    }
    match row.lifecycle {
        AgentEndpointLifecycle::Detached => {
            return Err(CommunicationStoreError::new(
                "endpoint_detached",
                "Agent Endpoint is detached",
            ));
        }
        AgentEndpointLifecycle::Expired => {
            return Err(CommunicationStoreError::new(
                "endpoint_expired",
                "Agent Endpoint is expired or stale",
            ));
        }
        AgentEndpointLifecycle::Attached => {}
    }
    if row.lease_expires_at_unix_ms <= now_unix_ms() {
        return Err(CommunicationStoreError::new(
            "endpoint_expired",
            "Agent Endpoint lease has expired",
        ));
    }
    let current_controller_generation: i64 = conn
        .query_row(
            "SELECT current_controller_generation FROM wc_agent_identities WHERE agent_id = ?1",
            params![agent_id],
            |row| row.get(0),
        )
        .map_err(store_error)?;
    if row.controller_generation != current_controller_generation
        || expected_controller_generation
            .is_some_and(|generation| generation != row.controller_generation)
    {
        return Err(CommunicationStoreError::new(
            "endpoint_generation_stale",
            "Agent Endpoint controller generation is stale",
        ));
    }
    Ok(row)
}

pub(super) fn load_agent(
    conn: &Connection,
    agent_id: &str,
) -> Result<Option<DurableAgentIdentity>, CommunicationStoreError> {
    conn.query_row(
        "SELECT agent_id, handle, display_name, description, specialty_labels_json,
                profile_revision, created_at_unix_ms, updated_at_unix_ms,
                current_controller_generation,
                (SELECT COUNT(*) FROM wc_agent_endpoints e
                 WHERE e.agent_id = a.agent_id AND e.lifecycle = 'attached'
                   AND e.controller_generation = a.current_controller_generation
                   AND e.lease_expires_at_unix_ms > ?2),
                (SELECT COUNT(*) FROM wc_agent_deliveries d
                 WHERE d.recipient_agent_id = a.agent_id AND d.state = 'queued'),
                (SELECT COUNT(*) FROM wc_agent_wakes w
                 WHERE w.target_agent_id = a.agent_id AND w.state != 'consumed'),
                (SELECT w.wake_id FROM wc_agent_wakes w
                 WHERE w.target_agent_id = a.agent_id
                 ORDER BY w.created_at_unix_ms DESC, w.wake_id DESC LIMIT 1),
                (SELECT w.state FROM wc_agent_wakes w
                 WHERE w.target_agent_id = a.agent_id
                 ORDER BY w.created_at_unix_ms DESC, w.wake_id DESC LIMIT 1)
         FROM wc_agent_identities a WHERE agent_id = ?1",
        params![agent_id, now_unix_ms()],
        row_to_agent,
    )
    .optional()
    .map_err(store_error)
}

fn row_to_agent(row: &rusqlite::Row<'_>) -> rusqlite::Result<DurableAgentIdentity> {
    let labels_json: String = row.get(4)?;
    let specialty_labels = serde_json::from_str(&labels_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(4, Type::Text, Box::new(error))
    })?;
    Ok(DurableAgentIdentity {
        agent_id: row.get(0)?,
        handle: row.get(1)?,
        display_name: row.get(2)?,
        description: row.get(3)?,
        specialty_labels,
        profile_revision: row.get(5)?,
        created_at_unix_ms: row.get(6)?,
        updated_at_unix_ms: row.get(7)?,
        current_controller_generation: row.get(8)?,
        active_endpoint_count: row.get(9)?,
        queued_delivery_count: row.get(10)?,
        unresolved_wake_count: row.get(11)?,
        latest_wake_id: row.get(12)?,
        latest_wake_state: row.get(13)?,
    })
}

pub(super) fn load_endpoint(
    conn: &Connection,
    endpoint_id: &str,
) -> Result<Option<AgentEndpointRecord>, CommunicationStoreError> {
    conn.query_row(
        "SELECT endpoint_id, agent_id, host, client_attachment_id, wake_capable,
                controller_generation, lifecycle, attached_at_unix_ms,
                last_seen_at_unix_ms, lease_expires_at_unix_ms,
                expired_at_unix_ms, detached_at_unix_ms
         FROM wc_agent_endpoints WHERE endpoint_id = ?1",
        params![endpoint_id],
        row_to_endpoint,
    )
    .optional()
    .map_err(store_error)
}

fn load_endpoint_for_principal(
    conn: &Connection,
    principal: &CommunicationPrincipal,
    endpoint_id: &str,
) -> Result<Option<AgentEndpointRecord>, CommunicationStoreError> {
    conn.query_row(
        "SELECT endpoint_id, agent_id, host, client_attachment_id, wake_capable,
                controller_generation, lifecycle, attached_at_unix_ms,
                last_seen_at_unix_ms, lease_expires_at_unix_ms,
                expired_at_unix_ms, detached_at_unix_ms
         FROM wc_agent_endpoints
         WHERE endpoint_id = ?1
           AND attachment_principal_kind = ?2
           AND attachment_principal_digest = ?3",
        params![endpoint_id, principal.kind, principal.digest],
        row_to_endpoint,
    )
    .optional()
    .map_err(store_error)
}

fn row_to_endpoint(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentEndpointRecord> {
    Ok(AgentEndpointRecord {
        endpoint_id: row.get(0)?,
        agent_id: row.get(1)?,
        host: row.get(2)?,
        client_attachment_id: row.get(3)?,
        wake_capable: row.get::<_, i64>(4)? != 0,
        controller_generation: row.get(5)?,
        lifecycle: AgentEndpointLifecycle::from_db(&row.get::<_, String>(6)?, 6)?,
        attached_at_unix_ms: row.get(7)?,
        last_seen_at_unix_ms: row.get(8)?,
        lease_expires_at_unix_ms: row.get(9)?,
        expired_at_unix_ms: row.get(10)?,
        detached_at_unix_ms: row.get(11)?,
    })
}

fn row_to_conversation_summary(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<ConversationSummaryRecord> {
    Ok(ConversationSummaryRecord {
        conversation_id: row.get(0)?,
        title: row.get(1)?,
        lifecycle: ConversationLifecycle::from_db(&row.get::<_, String>(2)?, 2)?,
        created_at_unix_ms: row.get(3)?,
        updated_at_unix_ms: row.get(4)?,
        participant_count: row.get(5)?,
        message_count: row.get(6)?,
        last_seq: row.get(7)?,
        queued_delivery_count: row.get(8)?,
    })
}

fn load_message(
    conn: &Connection,
    message_id: &str,
) -> Result<Option<ConversationMessageRecord>, CommunicationStoreError> {
    let base: Option<(
        String,
        String,
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        i64,
    )> = conn
        .query_row(
            "SELECT m.message_id, m.conversation_id, m.seq,
                    p.participant_kind, p.agent_id, a.handle, a.display_name,
                    p.principal_kind, m.body, m.reply_to_message_id,
                    m.created_at_unix_ms
             FROM wc_conversation_messages m
             JOIN wc_conversation_participants p
               ON p.participant_id = m.author_participant_id
             LEFT JOIN wc_agent_identities a ON a.agent_id = p.agent_id
             WHERE m.message_id = ?1",
            params![message_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                ))
            },
        )
        .optional()
        .map_err(store_error)?;
    let Some((
        message_id,
        conversation_id,
        seq,
        participant_kind,
        agent_id,
        handle,
        display_name,
        principal_kind,
        body,
        reply_to,
        created_at_unix_ms,
    )) = base
    else {
        return Ok(None);
    };
    let mut delivery_statement = conn
        .prepare(
            "SELECT delivery_order, delivery_id, recipient_agent_id, state,
                    created_at_unix_ms, consumed_at_unix_ms
             FROM wc_agent_deliveries
             WHERE message_id = ?1
             ORDER BY recipient_agent_id, delivery_id",
        )
        .map_err(store_error)?;
    let deliveries = delivery_statement
        .query_map(params![message_id], |row| {
            Ok(MessageDeliveryRecord {
                delivery_order: row.get(0)?,
                delivery_id: row.get(1)?,
                recipient_agent_id: row.get(2)?,
                state: MessageDeliveryState::from_db(&row.get::<_, String>(3)?, 3)?,
                created_at_unix_ms: row.get(4)?,
                consumed_at_unix_ms: row.get(5)?,
            })
        })
        .map_err(store_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(store_error)?;
    Ok(Some(ConversationMessageRecord {
        message_id,
        conversation_id,
        seq,
        author: MessageAuthorRecord {
            participant_kind,
            agent_id,
            handle,
            display_name,
            principal_kind,
        },
        body,
        reply_to,
        created_at_unix_ms,
        deliveries,
    }))
}

pub(super) fn lookup_idempotent_resource(
    transaction: &Transaction<'_>,
    principal: &CommunicationPrincipal,
    operation: &str,
    idempotency_key: &str,
    request_hash: &str,
) -> Result<Option<String>, CommunicationStoreError> {
    let key_hash = digest_text("webcodex.communication.idempotency-key.v1", idempotency_key);
    let existing: Option<(String, String)> = transaction
        .query_row(
            "SELECT request_hash, resource_id FROM wc_communication_idempotency
             WHERE principal_digest = ?1 AND operation = ?2 AND key_hash = ?3",
            params![principal.digest, operation, key_hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(store_error)?;
    match existing {
        None => Ok(None),
        Some((existing_request_hash, resource_id)) if existing_request_hash == request_hash => {
            Ok(Some(resource_id))
        }
        Some(_) => Err(CommunicationStoreError::new(
            "communication_idempotency_conflict",
            "Idempotency key was already used with a different request",
        )),
    }
}

pub(super) fn record_idempotent_resource(
    transaction: &Transaction<'_>,
    principal: &CommunicationPrincipal,
    operation: &str,
    idempotency_key: &str,
    request_hash: &str,
    resource_id: &str,
    now: i64,
) -> Result<(), CommunicationStoreError> {
    let key_hash = digest_text("webcodex.communication.idempotency-key.v1", idempotency_key);
    transaction
        .execute(
            "INSERT INTO wc_communication_idempotency (
                principal_digest, operation, key_hash, request_hash,
                resource_id, created_at_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                principal.digest,
                operation,
                key_hash,
                request_hash,
                resource_id,
                now,
            ],
        )
        .map_err(store_error)?;
    Ok(())
}

pub(super) fn validate_communication_principal(
    principal: &CommunicationPrincipal,
) -> Result<(), CommunicationStoreError> {
    let kind = principal.kind.trim();
    if kind.is_empty()
        || kind.chars().count() > MAX_COMMUNICATION_PRINCIPAL_KIND_CHARS
        || !kind.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | ':' | '.')
        })
    {
        return Err(CommunicationStoreError::new(
            "invalid_communication_principal",
            "Communication principal kind is invalid",
        ));
    }
    validate_digest_id(
        &principal.digest,
        COMMUNICATION_PRINCIPAL_DIGEST_PREFIX,
        "invalid_communication_principal",
    )
}

fn validate_digest_id(
    value: &str,
    prefix: &str,
    code: &'static str,
) -> Result<(), CommunicationStoreError> {
    let suffix = value.strip_prefix(prefix).ok_or_else(|| {
        CommunicationStoreError::new(code, "Communication principal digest is invalid")
    })?;
    if suffix.len() != 64
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(CommunicationStoreError::new(
            code,
            "Communication principal digest is invalid",
        ));
    }
    Ok(())
}

fn canonicalize_agent_ids(
    values: Vec<String>,
    require_nonempty: bool,
) -> Result<Vec<String>, CommunicationStoreError> {
    if values.len() > MAX_CONVERSATION_AGENT_PARTICIPANTS {
        return Err(CommunicationStoreError::new(
            "conversation_participant_limit_exceeded",
            format!(
                "A Conversation supports at most {MAX_CONVERSATION_AGENT_PARTICIPANTS} Agent participants"
            ),
        ));
    }
    let mut canonical = BTreeSet::new();
    for value in values {
        let value = value.trim().to_string();
        validate_id(&value, DURABLE_AGENT_ID_PREFIX, "invalid_agent_id")?;
        canonical.insert(value);
    }
    if require_nonempty && canonical.is_empty() {
        return Err(CommunicationStoreError::new(
            "conversation_agent_participant_required",
            "A Conversation requires at least one Agent participant",
        ));
    }
    Ok(canonical.into_iter().collect())
}

fn canonicalize_delivery_ids(values: Vec<String>) -> Result<Vec<String>, CommunicationStoreError> {
    if values.is_empty() || values.len() > MAX_DELIVERY_CONSUME_ITEMS {
        return Err(CommunicationStoreError::new(
            "invalid_delivery_ids",
            format!("delivery_ids must contain 1..={MAX_DELIVERY_CONSUME_ITEMS} values"),
        ));
    }
    let mut canonical = BTreeSet::new();
    for value in values {
        let value = value.trim().to_string();
        validate_id(&value, AGENT_DELIVERY_ID_PREFIX, "invalid_delivery_id")?;
        canonical.insert(value);
    }
    Ok(canonical.into_iter().collect())
}

fn canonicalize_specialty_labels(
    labels: Vec<String>,
) -> Result<Vec<String>, CommunicationStoreError> {
    if labels.len() > MAX_AGENT_SPECIALTY_LABELS {
        return Err(CommunicationStoreError::new(
            "agent_specialty_label_limit_exceeded",
            format!("Agent Card supports at most {MAX_AGENT_SPECIALTY_LABELS} specialty labels"),
        ));
    }
    let mut canonical = BTreeSet::new();
    for label in labels {
        let label = label.trim();
        if label.is_empty() || label.chars().count() > MAX_AGENT_SPECIALTY_LABEL_CHARS {
            return Err(CommunicationStoreError::new(
                "invalid_agent_specialty_label",
                format!(
                    "Agent specialty labels must contain 1..={MAX_AGENT_SPECIALTY_LABEL_CHARS} characters"
                ),
            ));
        }
        canonical.insert(label.to_string());
    }
    Ok(canonical.into_iter().collect())
}

fn validate_handle(value: &str) -> Result<String, CommunicationStoreError> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().count() > MAX_AGENT_HANDLE_CHARS
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
    {
        return Err(CommunicationStoreError::new(
            "invalid_agent_handle",
            format!(
                "Agent handle must use 1..={MAX_AGENT_HANDLE_CHARS} ASCII letters, digits, '.', '_' or '-'"
            ),
        ));
    }
    Ok(value.to_string())
}

fn validate_display_name(value: &str) -> Result<String, CommunicationStoreError> {
    validate_nonempty_chars(
        value,
        MAX_AGENT_DISPLAY_NAME_CHARS,
        "invalid_agent_display_name",
        "Agent display_name",
    )
}

fn validate_description(value: &str) -> Result<String, CommunicationStoreError> {
    let value = value.trim();
    if value.len() > MAX_AGENT_DESCRIPTION_BYTES {
        return Err(CommunicationStoreError::new(
            "invalid_agent_description",
            format!("Agent description exceeds {MAX_AGENT_DESCRIPTION_BYTES} UTF-8 bytes"),
        ));
    }
    Ok(value.to_string())
}

fn validate_message_body(value: &str) -> Result<String, CommunicationStoreError> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_CONVERSATION_MESSAGE_BYTES {
        return Err(CommunicationStoreError::new(
            "invalid_conversation_message_body",
            format!(
                "Conversation Message body must contain 1..={MAX_CONVERSATION_MESSAGE_BYTES} UTF-8 bytes"
            ),
        ));
    }
    Ok(value.to_string())
}

pub(super) fn validate_idempotency_key(value: &str) -> Result<String, CommunicationStoreError> {
    validate_nonempty_chars(
        value,
        MAX_COMMUNICATION_IDEMPOTENCY_KEY_CHARS,
        "invalid_communication_idempotency_key",
        "idempotency_key",
    )
}

fn validate_nonempty_chars(
    value: &str,
    max_chars: usize,
    code: &'static str,
    label: &str,
) -> Result<String, CommunicationStoreError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > max_chars {
        return Err(CommunicationStoreError::new(
            code,
            format!("{label} must contain 1..={max_chars} characters"),
        ));
    }
    Ok(value.to_string())
}

fn validate_optional_chars(
    value: Option<&str>,
    max_chars: usize,
    code: &'static str,
    label: &str,
) -> Result<Option<String>, CommunicationStoreError> {
    match value {
        None => Ok(None),
        Some(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Ok(None);
            }
            if value.chars().count() > max_chars {
                return Err(CommunicationStoreError::new(
                    code,
                    format!("{label} must contain at most {max_chars} characters"),
                ));
            }
            Ok(Some(value.to_string()))
        }
    }
}

fn bounded_limit(limit: usize) -> Result<usize, CommunicationStoreError> {
    if limit == 0 || limit > MAX_COMMUNICATION_LIST_LIMIT {
        return Err(CommunicationStoreError::new(
            "invalid_communication_list_limit",
            format!("limit must be within 1..={MAX_COMMUNICATION_LIST_LIMIT}"),
        ));
    }
    Ok(limit)
}

pub(super) fn validate_id(
    value: &str,
    prefix: &str,
    code: &'static str,
) -> Result<(), CommunicationStoreError> {
    let suffix = value.strip_prefix(prefix).ok_or_else(|| {
        CommunicationStoreError::new(code, format!("Invalid canonical id: {value}"))
    })?;
    if suffix.len() != 32
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(CommunicationStoreError::new(
            code,
            format!("Invalid canonical id: {value}"),
        ));
    }
    Ok(())
}

pub(super) fn new_id(prefix: &str) -> String {
    format!("{prefix}{}", Uuid::new_v4().simple())
}

fn digest_json(value: &Value) -> String {
    digest_text(
        "webcodex.communication.request.v1",
        &serde_json::to_string(value).expect("communication request serializes"),
    )
}

pub(super) fn digest_text(domain: &str, value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update(b"\0");
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub(super) fn now_unix_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod lifecycle_contract_tests {
    use super::*;

    #[test]
    fn durable_communication_lifecycle_encodings_are_closed_and_serde_stable() {
        for (lifecycle, db) in [
            (AgentEndpointLifecycle::Attached, "attached"),
            (AgentEndpointLifecycle::Detached, "detached"),
            (AgentEndpointLifecycle::Expired, "expired"),
        ] {
            assert_eq!(lifecycle.as_db(), db);
            assert_eq!(AgentEndpointLifecycle::from_db(db, 0).unwrap(), lifecycle);
            assert_eq!(serde_json::to_value(lifecycle).unwrap(), db);
        }
        assert!(AgentEndpointLifecycle::from_db("future_state", 0).is_err());

        for (lifecycle, db) in [
            (ConversationLifecycle::Open, "open"),
            (ConversationLifecycle::Closed, "closed"),
        ] {
            assert_eq!(lifecycle.as_db(), db);
            assert_eq!(ConversationLifecycle::from_db(db, 0).unwrap(), lifecycle);
            assert_eq!(serde_json::to_value(lifecycle).unwrap(), db);
        }
        assert!(ConversationLifecycle::from_db("future_state", 0).is_err());

        for (state, db) in [
            (MessageDeliveryState::Queued, "queued"),
            (MessageDeliveryState::Consumed, "consumed"),
        ] {
            assert_eq!(state.as_db(), db);
            assert_eq!(MessageDeliveryState::from_db(db, 0).unwrap(), state);
            assert_eq!(serde_json::to_value(state).unwrap(), db);
        }
        assert!(MessageDeliveryState::from_db("future_state", 0).is_err());
    }
}
