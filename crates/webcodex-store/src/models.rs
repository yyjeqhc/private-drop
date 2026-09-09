use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionSessionRecord {
    pub session_id: String,
    pub title: Option<String>,
    pub note: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub closed_at: Option<i64>,
    pub first_event_at: Option<i64>,
    pub last_event_at: Option<i64>,
    pub total_actions: i64,
    pub success_count: i64,
    pub failed_count: i64,
    pub timeout_or_unknown_count: i64,
    pub warning_count: i64,
    pub total_duration_ms: i64,
    pub changed_files_count: i64,
    pub job_ids_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionEventRecord {
    pub event_id: String,
    pub session_id: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub duration_ms: i64,
    pub endpoint: String,
    pub operation: Option<String>,
    pub action_name: String,
    pub project: Option<String>,
    /// Auth credential class observed at request time; `None` for legacy or
    /// unauthenticated events whose caller cannot be established.
    pub principal_kind: Option<String>,
    /// Stable authenticated WebCodex user id when the credential is user-bound.
    pub principal_user_id: Option<String>,
    /// Stable OAuth client id for OAuth2 access-token calls only.
    pub oauth_client_id: Option<String>,
    pub status: String,
    pub http_status: Option<i64>,
    pub error_summary: Option<String>,
    pub warning_summary: Option<String>,
    pub changed_files_json: String,
    pub ids_json: String,
    pub summary_json: String,
    pub request_bytes: Option<i64>,
    pub response_bytes: Option<i64>,
    /// Domain-separated hashed host Window identity. Raw host/session values
    /// never cross the adapter boundary into durable storage.
    #[serde(default)]
    pub client_window_key: Option<String>,
    #[serde(default)]
    pub client_window_source: Option<String>,
    /// Safe Server request-trace correlation id. This is not a trace payload or
    /// filesystem reference.
    #[serde(default)]
    pub server_trace_id: Option<String>,
    /// Stable server-derived caller correlation used only to filter Window
    /// observability. It never grants authorization.
    #[serde(default)]
    pub principal_correlation_kind: Option<String>,
    #[serde(default)]
    pub principal_correlation_id: Option<String>,
    /// Millisecond-resolution Window activity timestamps. Existing ActionAudit
    /// started_at / ended_at fields deliberately remain seconds.
    #[serde(default)]
    pub window_started_at_ms: Option<i64>,
    #[serde(default)]
    pub window_ended_at_ms: Option<i64>,
    /// Canonical adapter request-observed and response-handoff timestamps.
    /// They are nullable so legacy events remain explicitly unavailable.
    #[serde(default)]
    pub request_observed_at_ms: Option<i64>,
    #[serde(default)]
    pub response_handed_at_ms: Option<i64>,
    /// `serial`, `overlap`, or `unavailable` when classified in-process.
    #[serde(default)]
    pub window_transition_kind: Option<String>,
    /// Null for legacy events; streaming handoff is not response completion.
    #[serde(default)]
    pub response_streaming: Option<bool>,
    #[serde(default)]
    pub window_continuity_eligible: Option<bool>,
    #[serde(default)]
    pub window_meaningful: bool,
    /// Diagnostic candidate for an omitted outer Workflow Session recorder.
    /// This is observation evidence only and never a Session link/authority.
    #[serde(default)]
    pub recorder_gap_session_id: Option<String>,
}

/// Authoritative correlation evidence attached to one ActionAudit event. An
/// event may carry multiple rows (for example an outer recorder plus a newly
/// created work_on_project Session), so Window↔Session semantics stay genuinely
/// many-to-many.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionEventWorkflowLinkRecord {
    pub event_id: String,
    pub workflow_session_id: String,
    pub workflow_session_relation: String,
    pub project: Option<String>,
    pub linked_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowWorkflowLinkRecord {
    pub workflow_session_id: String,
    pub project: Option<String>,
    pub relation: String,
    pub linked_at_ms: i64,
}

/// Payload-safe durable Window activity row used by Runtime Console queries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowActivityEventRecord {
    pub event_id: String,
    pub client_window_key: String,
    pub client_window_source: String,
    pub server_trace_id: Option<String>,
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    pub duration_ms: i64,
    pub action_name: String,
    pub operation: Option<String>,
    pub project: Option<String>,
    pub status: String,
    pub meaningful: bool,
    pub recorder_gap_session_id: Option<String>,
    pub workflow_links: Vec<WindowWorkflowLinkRecord>,
    /// Internal principal correlation is intentionally not serialized by the
    /// Runtime Console projection. It is used only to avoid cross-principal
    /// adjacency when an administrator reads a Window.
    #[serde(skip)]
    pub principal_correlation_kind: Option<String>,
    #[serde(skip)]
    pub principal_correlation_id: Option<String>,
    pub request_observed_at_ms: Option<i64>,
    pub response_handed_at_ms: Option<i64>,
    pub window_transition_kind: Option<String>,
    pub response_streaming: Option<bool>,
    pub window_continuity_eligible: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowActivitySummaryRecord {
    pub client_window_key: String,
    pub client_window_source: String,
    pub last_seen_at_ms: i64,
    pub last_tool_call_at_ms: Option<i64>,
    pub last_meaningful_activity_at_ms: Option<i64>,
    pub linked_session_count: usize,
    pub recorder_gap_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowSessionLinkSummaryRecord {
    pub client_window_key: String,
    pub client_window_source: String,
    pub first_linked_at_ms: i64,
    pub last_linked_at_ms: i64,
    pub relation_count: usize,
    pub relations: Vec<String>,
    pub last_seen_at_ms: i64,
    pub last_meaningful_activity_at_ms: Option<i64>,
    pub recorder_gap_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowWorkflowSessionSummaryRecord {
    pub workflow_session_id: String,
    pub project: Option<String>,
    pub first_linked_at_ms: i64,
    pub last_linked_at_ms: i64,
    pub relation_count: usize,
    pub relations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowWorkflowAffinityRecord {
    pub workflow_session_id: String,
    pub project: Option<String>,
    pub relation: String,
    pub linked_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
    pub id: String,
    pub username: String,
    pub created_at: i64,
    pub disabled: i64,
    /// Optional human-readable name. Phase 2 user model.
    #[serde(default)]
    pub display_name: Option<String>,
    /// `"admin"` or `"user"`. Defaults to `"user"` for legacy rows.
    #[serde(default = "default_user_role")]
    pub role: String,
    /// Optional unix timestamp marking when the user was disabled. Mirrors
    /// the legacy `disabled` flag (disabled != 0 implies disabled_at is set).
    #[serde(default)]
    pub disabled_at: Option<i64>,
    /// Optional unix timestamp of the most recent user metadata update.
    #[serde(default)]
    pub updated_at: Option<i64>,
}

fn default_user_role() -> String {
    "user".to_string()
}

impl UserRecord {
    /// True when the user is disabled (disabled flag set or disabled_at present).
    pub fn is_disabled(&self) -> bool {
        self.disabled != 0 || self.disabled_at.is_some()
    }

    /// True when the user holds the admin role.
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }
}

/// The kind of an API key. Phase 3 introduces agent tokens that are bound to
/// an owner username and an allowed `client_id`, and may only be used on agent
/// transport endpoints. Existing Phase 2 personal API tokens default to
/// `user`.
pub const TOKEN_KIND_USER: &str = "user";
pub const TOKEN_KIND_AGENT: &str = "agent";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub key_prefix: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
    /// Space-separated scope list (e.g. `"runtime:read project:write"`).
    /// Empty string means no scopes. Phase 2 token model.
    #[serde(default)]
    pub scopes: String,
    /// Optional unix timestamp after which the token must no longer
    /// authenticate. `None` means the token never expires.
    #[serde(default)]
    pub expires_at: Option<i64>,
    /// Phase 3 token kind: `"user"` (default) or `"agent"`. Agent tokens are
    /// bound to an owner username (via `user_id`) and an `allowed_client_id`,
    /// and may only authorize agent transport endpoints. Older rows that
    /// predate the column default to `"user"` via the DB schema and the
    /// [`ApiKeyRecord::kind`] helper.
    #[serde(default)]
    pub kind: String,
    /// Phase 3: the `client_id` an agent token is bound to. Required for
    /// agent tokens; `None` for user tokens. An agent token may only
    /// register/poll/result/job_update for a client_id that matches this
    /// value.
    #[serde(default)]
    pub allowed_client_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountCredentialRecord {
    pub id: String,
    pub user_id: String,
    pub credential_prefix: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingCodeRecord {
    pub id: String,
    pub code_hash: String,
    pub user_id: String,
    pub username: String,
    pub client_id: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub used_at: Option<i64>,
    pub user_token_name: Option<String>,
    pub agent_token_name: Option<String>,
}

impl ApiKeyRecord {
    /// Parse the stored scope string into an ordered, deduplicated list.
    pub fn scopes_vec(&self) -> Vec<String> {
        self.scopes.split_whitespace().map(str::to_string).collect()
    }

    /// True when the token has been explicitly revoked.
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    /// True when the token has expired relative to `now` (unix seconds).
    pub fn is_expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|exp| now >= exp)
    }

    /// The token kind, normalized to `"user"` when unset (legacy rows).
    pub fn kind(&self) -> &str {
        if self.kind.is_empty() {
            TOKEN_KIND_USER
        } else {
            self.kind.as_str()
        }
    }

    /// True when this is an agent token (kind == "agent").
    pub fn is_agent_token(&self) -> bool {
        self.kind() == TOKEN_KIND_AGENT
    }

    /// True when this is a user token (kind == "user", the default).
    pub fn is_user_token(&self) -> bool {
        self.kind() == TOKEN_KIND_USER
    }

    /// The `allowed_client_id` bound to an agent token. `None` for user tokens.
    pub fn allowed_client_id(&self) -> Option<&str> {
        self.allowed_client_id.as_deref()
    }
}

// ---------------------------------------------------------------------------
// OAuth2 models — Phase 2a
// ---------------------------------------------------------------------------

/// OAuth2 registered client. The `client_secret` plaintext is returned only at
/// creation time; only `client_secret_hash` is stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthClientRecord {
    pub id: String,
    /// Opaque public identifier (e.g. `wc_client_<random>`).
    pub client_id: String,
    /// SHA-256 hash of the client secret. The plaintext is never stored.
    pub client_secret_hash: String,
    pub name: String,
    /// Managed-user owner for ordinary managed OAuth clients. Exactly one
    /// OAuth client owner field is present.
    pub owner_user_id: Option<String>,
    /// Project-grant owner for project-first share OAuth clients.
    pub owner_project_grant_id: Option<String>,
    /// SHA-256 group owner for hosted direct-shared-key OAuth bridge clients.
    /// The plaintext shared key is never stored in the OAuth client row.
    pub owner_shared_key_hash: Option<String>,
    /// Newline-separated list of allowed redirect URIs.
    pub redirect_uris: String,
    /// Space-separated scope list (same format as PAT scopes).
    pub allowed_scopes: String,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
}

/// One-time authorization code. Short-lived (default 300s), single-use.
/// PKCE fields (`code_challenge`, `code_challenge_method`) are stored for S256
/// validation. `resource` is reserved for MCP audience binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthAuthorizationCodeRecord {
    pub id: String,
    /// SHA-256 hash of the code. Plaintext never stored.
    pub code_hash: String,
    pub client_id: String,
    /// Stable OAuth subject discriminator. Supported values are
    /// `"managed_user"` and `"shared_key"`.
    pub subject_kind: String,
    /// Stable subject identifier: managed users use `user_id`; shared-key
    /// subjects use `shared_key_hash`.
    pub subject_id: String,
    pub user_id: Option<String>,
    pub redirect_uri: String,
    pub scopes: String,
    /// PKCE code challenge (S256). `None` when PKCE is not used.
    pub code_challenge: Option<String>,
    /// PKCE method. Only `"S256"` is supported. `None` when PKCE is not used.
    pub code_challenge_method: Option<String>,
    /// Audience / resource indicator for MCP OAuth. `None` when not specified.
    pub resource: Option<String>,
    /// Optional shared-key group hash for future bridge-issued authorization
    /// codes. Plaintext shared keys are never stored.
    pub shared_key_hash: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    pub used_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

/// Short-lived OAuth2 access token (default 3600s). Only the SHA-256 hash is
/// stored; the plaintext is returned to the client once at creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthAccessTokenRecord {
    pub id: String,
    /// SHA-256 hash of the token. Plaintext never stored.
    pub token_hash: String,
    pub client_id: String,
    /// Stable OAuth subject discriminator. Supported values are
    /// `"managed_user"` and `"shared_key"`.
    pub subject_kind: String,
    /// Stable subject identifier: managed users use `user_id`; shared-key
    /// subjects use `shared_key_hash`.
    pub subject_id: String,
    pub user_id: Option<String>,
    pub scopes: String,
    /// Audience / resource indicator for MCP OAuth.
    pub resource: Option<String>,
    /// Optional shared-key group hash for internally issued bridge tokens.
    /// Plaintext shared keys are never stored.
    pub shared_key_hash: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    pub revoked_at: Option<i64>,
    pub last_used_at: Option<i64>,
}

/// Long-lived OAuth2 refresh token (default 30 days). Supports rotation via
/// `rotated_from_id`. Only the SHA-256 hash is stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthRefreshTokenRecord {
    pub id: String,
    /// SHA-256 hash of the token. Plaintext never stored.
    pub token_hash: String,
    pub client_id: String,
    /// Stable OAuth subject discriminator. Supported values are
    /// `"managed_user"` and `"shared_key"`.
    pub subject_kind: String,
    /// Stable subject identifier: managed users use `user_id`; shared-key
    /// subjects use `shared_key_hash`.
    pub subject_id: String,
    pub user_id: Option<String>,
    pub scopes: String,
    /// Audience / resource indicator for MCP OAuth.
    pub resource: Option<String>,
    /// Optional shared-key group hash inherited by rotated bridge tokens.
    /// Plaintext shared keys are never stored.
    pub shared_key_hash: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    pub revoked_at: Option<i64>,
    pub last_used_at: Option<i64>,
    /// When this token was created via refresh token rotation, points to the
    /// previous refresh token. `None` for the initial token.
    pub rotated_from_id: Option<String>,
}

impl OAuthAccessTokenRecord {
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }

    pub fn scopes_vec(&self) -> Vec<String> {
        self.scopes.split_whitespace().map(str::to_string).collect()
    }
}

impl OAuthRefreshTokenRecord {
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }

    pub fn scopes_vec(&self) -> Vec<String> {
        self.scopes.split_whitespace().map(str::to_string).collect()
    }
}

impl OAuthAuthorizationCodeRecord {
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }

    pub fn is_used(&self) -> bool {
        self.used_at.is_some()
    }

    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    pub fn scopes_vec(&self) -> Vec<String> {
        self.scopes.split_whitespace().map(str::to_string).collect()
    }
}

impl OAuthClientRecord {
    pub fn is_managed_user_owned(&self) -> bool {
        self.owner_user_id.is_some()
            && self.owner_project_grant_id.is_none()
            && self.owner_shared_key_hash.is_none()
    }

    pub fn is_project_grant_owned(&self) -> bool {
        self.owner_user_id.is_none()
            && self.owner_project_grant_id.is_some()
            && self.owner_shared_key_hash.is_none()
    }

    pub fn is_shared_key_owned(&self) -> bool {
        self.owner_user_id.is_none()
            && self.owner_project_grant_id.is_none()
            && self.owner_shared_key_hash.is_some()
    }

    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    pub fn allowed_scopes_vec(&self) -> Vec<String> {
        self.allowed_scopes
            .split_whitespace()
            .map(str::to_string)
            .collect()
    }

    pub fn redirect_uris_vec(&self) -> Vec<String> {
        self.redirect_uris
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }
}

impl Default for ApiKeyRecord {
    /// Convenience default used by tests that build a record field-by-field;
    /// production code constructs the struct explicitly. Defaults `kind` to
    /// `"user"` and `allowed_client_id` to `None`.
    fn default() -> Self {
        Self {
            id: String::new(),
            user_id: String::new(),
            name: String::new(),
            key_prefix: String::new(),
            created_at: 0,
            last_used_at: None,
            revoked_at: None,
            scopes: String::new(),
            expires_at: None,
            kind: TOKEN_KIND_USER.to_string(),
            allowed_client_id: None,
        }
    }
}
