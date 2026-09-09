use super::{RecoveryKind, ToolResult, ToolRuntime};
use crate::auth::{AuthContext, AuthKind};
use crate::db::{
    AgentEndpointLifecycle, AgentProfilePatch, AgentWakeState, CommunicationPrincipal,
    CommunicationStoreError, ConversationAccess, NewAgentEndpoint, NewAgentIdentity,
    NewConversation, NewConversationMessage, COMMUNICATION_PRINCIPAL_DIGEST_PREFIX,
};
use serde::Serialize;
use serde_json::{json, to_value};
use sha2::{Digest, Sha256};

const DEFAULT_COMMUNICATION_LIST_LIMIT: usize = 50;

pub(super) fn communication_principal(
    auth: Option<&AuthContext>,
) -> Result<CommunicationPrincipal, ToolResult> {
    let (kind, subject) = match auth {
        None => ("local-development", "local-development".to_string()),
        Some(auth) if auth.is_oauth_shared_key_subject() => (
            "shared-key",
            auth.shared_key_hash.clone().ok_or_else(|| {
                communication_principal_unavailable(
                    "OAuth shared-key communication identity is missing its stable subject hash",
                )
            })?,
        ),
        Some(auth) => match auth.kind {
            AuthKind::Bootstrap => (
                "bootstrap",
                auth.user_id
                    .clone()
                    .or_else(|| auth.username.clone())
                    .unwrap_or_else(|| "bootstrap".to_string()),
            ),
            AuthKind::SharedKey => (
                "shared-key",
                auth.shared_key_hash.clone().ok_or_else(|| {
                    communication_principal_unavailable(
                        "Shared-key communication identity is missing its stable subject hash",
                    )
                })?,
            ),
            AuthKind::OpenAnonymous => ("open", "open-anonymous".to_string()),
            AuthKind::ApiToken | AuthKind::AccountCredential | AuthKind::OAuth2Token => {
                if auth.is_oauth_project_subject() {
                    return Err(communication_principal_unavailable(
                        "Project-scoped OAuth credentials cannot become durable communication principals",
                    ));
                }
                let subject = auth
                    .user_id
                    .clone()
                    .or_else(|| auth.username.clone())
                    .ok_or_else(|| {
                        communication_principal_unavailable(
                            "Managed communication credentials require a stable user identity",
                        )
                    })?;
                ("managed-user", subject)
            }
            AuthKind::AgentToken | AuthKind::ProjectCredential => {
                return Err(communication_principal_unavailable(
                    "Runner transport and Project credentials cannot become durable communication principals",
                ));
            }
        },
    };

    let mut hasher = Sha256::new();
    hasher.update(b"webcodex.communication.principal.v1\0");
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(subject.as_bytes());
    Ok(CommunicationPrincipal {
        kind: kind.to_string(),
        digest: format!(
            "{COMMUNICATION_PRINCIPAL_DIGEST_PREFIX}{:x}",
            hasher.finalize()
        ),
    })
}

fn communication_principal_unavailable(message: &str) -> ToolResult {
    ToolResult::err_with_output(
        message,
        json!({
            "error_kind": "communication_principal_unavailable",
            "state_changed": false,
        }),
    )
    .with_recovery(RecoveryKind::UserAction, None)
}

fn access_from_endpoint(
    agent_id: Option<String>,
    endpoint_id: Option<String>,
    expected_controller_generation: Option<i64>,
) -> Result<ConversationAccess, ToolResult> {
    match (
        agent_id,
        endpoint_id,
        expected_controller_generation,
    ) {
        (None, None, None) => Ok(ConversationAccess::Human),
        (Some(agent_id), Some(endpoint_id), Some(expected_controller_generation)) => {
            Ok(ConversationAccess::Agent {
            agent_id,
            endpoint_id,
                expected_controller_generation,
            })
        }
        _ => Err(ToolResult::err_with_output(
            "agent_id, endpoint_id, and expected_controller_generation must be provided together for an Agent conversation view",
            json!({
                "error_kind": "invalid_conversation_access",
                "state_changed": false,
            }),
        )
        .with_recovery(RecoveryKind::FixInput, None)),
    }
}

fn communication_store_unavailable() -> ToolResult {
    ToolResult::err_with_output(
        "Durable Agent and Conversation storage is unavailable in this runtime",
        json!({
            "error_kind": "communication_store_unavailable",
            "state_changed": false,
        }),
    )
    .with_recovery(RecoveryKind::UserAction, None)
}

fn communication_recovery_kind(
    error_kind: &str,
    store_failure_recovery: RecoveryKind,
) -> RecoveryKind {
    match error_kind {
        "communication_store_unavailable" => store_failure_recovery,
        "agent_profile_changed" => RecoveryKind::Reobserve,
        "endpoint_expired"
        | "endpoint_detached"
        | "endpoint_not_active"
        | "endpoint_generation_stale"
        | "wake_endpoint_fence_mismatch"
        | "wake_claim_stale" => RecoveryKind::Reconcile,
        _ => RecoveryKind::FixInput,
    }
}

fn communication_error(
    error: CommunicationStoreError,
    store_failure_recovery: RecoveryKind,
) -> ToolResult {
    let recovery = communication_recovery_kind(error.code(), store_failure_recovery);
    ToolResult::err_with_output(
        error.message(),
        json!({
            "error_kind": error.code(),
            "message": error.message(),
            "current_profile_revision": error.current_profile_revision(),
            "state_changed": false,
        }),
    )
    .with_recovery(recovery, None)
}

fn serialized_success<T: Serialize>(value: T) -> ToolResult {
    match to_value(value) {
        Ok(value) => ToolResult::ok(value),
        Err(error) => ToolResult::err_with_output(
            format!("Failed to serialize durable communication result: {error}"),
            json!({
                "error_kind": "communication_result_serialization_failed",
                "state_changed": false,
            }),
        )
        .with_recovery(RecoveryKind::NoAction, None),
    }
}

impl ToolRuntime {
    pub(crate) fn create_agent_identity(
        &self,
        auth: Option<&AuthContext>,
        handle: String,
        display_name: String,
        description: Option<String>,
        specialty_labels: Vec<String>,
        idempotency_key: String,
    ) -> ToolResult {
        let principal = match communication_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(db) = self.communication_db.as_ref() else {
            return communication_store_unavailable();
        };
        match db.create_agent_identity(
            &principal,
            NewAgentIdentity {
                handle,
                display_name,
                description: description.unwrap_or_default(),
                specialty_labels,
                idempotency_key,
            },
        ) {
            Ok(result) => serialized_success(result),
            Err(error) => communication_error(error, RecoveryKind::RetrySame),
        }
    }

    pub(crate) fn list_agent_identities(
        &self,
        auth: Option<&AuthContext>,
        agent_id: Option<String>,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> ToolResult {
        let principal = match communication_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(db) = self.communication_db.as_ref() else {
            return communication_store_unavailable();
        };
        match db.list_agent_identities(
            &principal,
            agent_id.as_deref(),
            offset.unwrap_or(0),
            limit.unwrap_or(DEFAULT_COMMUNICATION_LIST_LIMIT),
        ) {
            Ok(result) => serialized_success(result),
            Err(error) => communication_error(error, RecoveryKind::RetrySame),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update_agent_identity(
        &self,
        auth: Option<&AuthContext>,
        agent_id: String,
        expected_profile_revision: i64,
        handle: Option<String>,
        display_name: Option<String>,
        description: Option<String>,
        specialty_labels: Option<Vec<String>>,
    ) -> ToolResult {
        let principal = match communication_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(db) = self.communication_db.as_ref() else {
            return communication_store_unavailable();
        };
        match db.update_agent_identity(
            &principal,
            &agent_id,
            expected_profile_revision,
            AgentProfilePatch {
                handle,
                display_name,
                description,
                specialty_labels,
            },
        ) {
            Ok(result) => serialized_success(result),
            Err(error) => communication_error(error, RecoveryKind::Reconcile),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn attach_agent_endpoint(
        &self,
        auth: Option<&AuthContext>,
        agent_id: String,
        host: String,
        client_attachment_id: Option<String>,
        idempotency_key: String,
    ) -> ToolResult {
        let principal = match communication_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(db) = self.communication_db.as_ref() else {
            return communication_store_unavailable();
        };
        match db.attach_agent_endpoint(
            &principal,
            NewAgentEndpoint {
                agent_id,
                host,
                client_attachment_id,
                // Public/model/Console attachment cannot self-assert Host
                // continuation capability. Only process-local adapter
                // registration may transition this field to true.
                wake_capable: false,
                idempotency_key,
            },
        ) {
            Ok(result) => {
                if result.state_changed {
                    if let Some(controller) = self.agent_continuations.as_ref() {
                        controller.reconcile_attached_endpoint(
                            &result.endpoint.agent_id,
                            &result.endpoint.endpoint_id,
                            result.endpoint.controller_generation,
                        );
                    }
                }
                serialized_success(result)
            }
            Err(error) => communication_error(error, RecoveryKind::RetrySame),
        }
    }

    pub(crate) fn renew_agent_endpoint(
        &self,
        auth: Option<&AuthContext>,
        endpoint_id: String,
        expected_controller_generation: i64,
    ) -> ToolResult {
        let principal = match communication_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(db) = self.communication_db.as_ref() else {
            return communication_store_unavailable();
        };
        match db.renew_agent_endpoint(&principal, &endpoint_id, expected_controller_generation) {
            Ok(result) => serialized_success(result),
            Err(error) => communication_error(error, RecoveryKind::Reconcile),
        }
    }

    /// Host-integration boundary for registering a callable continuation
    /// adapter after an explicit Endpoint attach. This is intentionally not a
    /// model-facing tool: callback handles are process-local Host state.
    #[allow(dead_code)]
    pub(crate) fn register_agent_continuation_adapter(
        &self,
        auth: Option<&AuthContext>,
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
        adapter: std::sync::Arc<dyn crate::agent_wake::ContinuationAdapter>,
    ) -> ToolResult {
        let principal = match communication_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(controller) = self.agent_continuations.as_ref() else {
            return communication_store_unavailable();
        };
        match controller.register_endpoint_adapter(
            principal,
            agent_id,
            endpoint_id,
            expected_controller_generation,
            adapter,
        ) {
            Ok(endpoint) => serialized_success(json!({
                "endpoint": endpoint,
                "adapter_registered": true,
                "state_changed": true,
            })),
            Err(error) => communication_error(error, RecoveryKind::Reconcile),
        }
    }

    /// Host-integration boundary for withdrawing one exact callable adapter
    /// while preserving the durable Endpoint, Inbox, and Wake state.
    #[allow(dead_code)]
    pub(crate) fn unregister_agent_continuation_adapter(
        &self,
        auth: Option<&AuthContext>,
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
    ) -> ToolResult {
        let principal = match communication_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(controller) = self.agent_continuations.as_ref() else {
            return communication_store_unavailable();
        };
        match controller.unregister_endpoint_adapter(
            &principal,
            &agent_id,
            &endpoint_id,
            expected_controller_generation,
        ) {
            Ok(endpoint) => serialized_success(json!({
                "endpoint": endpoint,
                "adapter_registered": false,
                "state_changed": true,
            })),
            Err(error) => communication_error(error, RecoveryKind::Reconcile),
        }
    }

    pub(crate) fn detach_agent_endpoint(
        &self,
        auth: Option<&AuthContext>,
        endpoint_id: String,
    ) -> ToolResult {
        let principal = match communication_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(db) = self.communication_db.as_ref() else {
            return communication_store_unavailable();
        };
        match db.detach_agent_endpoint(&principal, &endpoint_id) {
            Ok(result) => {
                if let Some(controller) = self.agent_continuations.as_ref() {
                    controller.endpoint_detached(
                        &result.endpoint.agent_id,
                        &result.endpoint.endpoint_id,
                        result.endpoint.controller_generation,
                    );
                }
                serialized_success(result)
            }
            Err(error) => communication_error(error, RecoveryKind::RetrySame),
        }
    }

    pub(crate) fn create_conversation(
        &self,
        auth: Option<&AuthContext>,
        title: Option<String>,
        agent_ids: Vec<String>,
        idempotency_key: String,
    ) -> ToolResult {
        let principal = match communication_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(db) = self.communication_db.as_ref() else {
            return communication_store_unavailable();
        };
        match db.create_conversation(
            &principal,
            NewConversation {
                title,
                agent_ids,
                idempotency_key,
            },
        ) {
            Ok(result) => serialized_success(result),
            Err(error) => communication_error(error, RecoveryKind::RetrySame),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn list_conversations(
        &self,
        auth: Option<&AuthContext>,
        agent_id: Option<String>,
        endpoint_id: Option<String>,
        expected_controller_generation: Option<i64>,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> ToolResult {
        let principal = match communication_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let access =
            match access_from_endpoint(agent_id, endpoint_id, expected_controller_generation) {
                Ok(access) => access,
                Err(result) => return result,
            };
        let Some(db) = self.communication_db.as_ref() else {
            return communication_store_unavailable();
        };
        match db.list_conversations(
            &principal,
            &access,
            offset.unwrap_or(0),
            limit.unwrap_or(DEFAULT_COMMUNICATION_LIST_LIMIT),
        ) {
            Ok(result) => serialized_success(result),
            Err(error) => communication_error(error, RecoveryKind::RetrySame),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn read_conversation(
        &self,
        auth: Option<&AuthContext>,
        conversation_id: String,
        agent_id: Option<String>,
        endpoint_id: Option<String>,
        expected_controller_generation: Option<i64>,
        after_seq: Option<i64>,
        limit: Option<usize>,
    ) -> ToolResult {
        let principal = match communication_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let access =
            match access_from_endpoint(agent_id, endpoint_id, expected_controller_generation) {
                Ok(access) => access,
                Err(result) => return result,
            };
        let Some(db) = self.communication_db.as_ref() else {
            return communication_store_unavailable();
        };
        match db.read_conversation(
            &principal,
            &access,
            &conversation_id,
            after_seq.unwrap_or(0),
            limit.unwrap_or(DEFAULT_COMMUNICATION_LIST_LIMIT),
        ) {
            Ok(result) => serialized_success(result),
            Err(error) => communication_error(error, RecoveryKind::RetrySame),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn post_conversation_message(
        &self,
        auth: Option<&AuthContext>,
        conversation_id: String,
        body: String,
        author_agent_id: Option<String>,
        endpoint_id: Option<String>,
        expected_controller_generation: Option<i64>,
        recipient_agent_ids: Option<Vec<String>>,
        reply_to: Option<String>,
        idempotency_key: Option<String>,
        wake_reply_id: Option<String>,
        reply_operation_index: Option<i64>,
    ) -> ToolResult {
        let principal = match communication_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(db) = self.communication_db.as_ref() else {
            return communication_store_unavailable();
        };
        match db.post_conversation_message(
            &principal,
            NewConversationMessage {
                conversation_id,
                body,
                author_agent_id,
                endpoint_id,
                expected_controller_generation,
                recipient_agent_ids,
                reply_to,
                idempotency_key,
                wake_reply_id,
                reply_operation_index,
            },
        ) {
            Ok(result) => {
                if result.state_changed {
                    if let Some(controller) = self.agent_continuations.as_ref() {
                        let mut recipients = result
                            .message
                            .deliveries
                            .iter()
                            .map(|delivery| delivery.recipient_agent_id.as_str())
                            .collect::<Vec<_>>();
                        recipients.sort_unstable();
                        recipients.dedup();
                        for recipient in recipients {
                            controller.schedule_agent(recipient);
                        }
                    }
                }
                serialized_success(result)
            }
            Err(error) => communication_error(error, RecoveryKind::RetrySame),
        }
    }

    pub(crate) fn list_agent_inbox(
        &self,
        auth: Option<&AuthContext>,
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
        after_delivery_order: Option<i64>,
        limit: Option<usize>,
    ) -> ToolResult {
        let principal = match communication_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(db) = self.communication_db.as_ref() else {
            return communication_store_unavailable();
        };
        match db.list_agent_inbox(
            &principal,
            &agent_id,
            &endpoint_id,
            expected_controller_generation,
            after_delivery_order.unwrap_or(0),
            limit.unwrap_or(DEFAULT_COMMUNICATION_LIST_LIMIT),
        ) {
            Ok(result) => serialized_success(result),
            Err(error) => communication_error(error, RecoveryKind::RetrySame),
        }
    }

    pub(crate) fn consume_agent_deliveries(
        &self,
        auth: Option<&AuthContext>,
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
        delivery_ids: Vec<String>,
    ) -> ToolResult {
        let principal = match communication_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(db) = self.communication_db.as_ref() else {
            return communication_store_unavailable();
        };
        match db.consume_agent_deliveries(
            &principal,
            &agent_id,
            &endpoint_id,
            expected_controller_generation,
            delivery_ids,
        ) {
            Ok(result) => serialized_success(result),
            Err(error) => communication_error(error, RecoveryKind::RetrySame),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn consume_agent_wake(
        &self,
        auth: Option<&AuthContext>,
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
        wake_id: String,
        consume_token: String,
    ) -> ToolResult {
        let principal = match communication_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(db) = self.communication_db.as_ref() else {
            return communication_store_unavailable();
        };
        match db.consume_agent_wake(
            &principal,
            &agent_id,
            &endpoint_id,
            expected_controller_generation,
            &wake_id,
            &consume_token,
        ) {
            Ok(result) => {
                if result.state_changed {
                    if let Some(controller) = self.agent_continuations.as_ref() {
                        controller.schedule_agent(&agent_id);
                    }
                }
                serialized_success(result)
            }
            Err(error) => communication_error(error, RecoveryKind::RetrySame),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bootstrap_agent_conversation(
        &self,
        auth: Option<&AuthContext>,
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
        conversation_id: Option<String>,
        wake_id: Option<String>,
        activation_idempotency_key: Option<String>,
    ) -> ToolResult {
        let principal = match communication_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(db) = self.communication_db.as_ref() else {
            return communication_store_unavailable();
        };
        let mut bootstrap = match db.bootstrap_agent_conversation(
            &principal,
            &agent_id,
            &endpoint_id,
            expected_controller_generation,
            conversation_id.as_deref(),
            wake_id.as_deref(),
        ) {
            Ok(bootstrap) => bootstrap,
            Err(error) => return communication_error(error, RecoveryKind::Reconcile),
        };
        let wake_activation =
            if let Some(activation_idempotency_key) = activation_idempotency_key.as_deref() {
                let Some(wake) = bootstrap.wake.as_ref() else {
                    return communication_error(
                        CommunicationStoreError::new(
                            "wake_not_found",
                            "No unresolved Agent Wake is available for explicit activation",
                        ),
                        RecoveryKind::Reconcile,
                    );
                };
                let activation = match db.accept_explicit_agent_wake_activation(
                    &principal,
                    &agent_id,
                    &endpoint_id,
                    expected_controller_generation,
                    &wake.wake_id,
                    activation_idempotency_key,
                ) {
                    Ok(activation) => activation,
                    Err(error) => return communication_error(error, RecoveryKind::Reconcile),
                };
                bootstrap = match db.bootstrap_agent_conversation(
                    &principal,
                    &agent_id,
                    &endpoint_id,
                    expected_controller_generation,
                    conversation_id.as_deref(),
                    Some(&activation.wake.wake_id),
                ) {
                    Ok(bootstrap) => bootstrap,
                    Err(error) => return communication_error(error, RecoveryKind::Reconcile),
                };
                Some(json!({
                    "wake_id": activation.wake.wake_id,
                    "attempt_id": activation.attempt_id,
                    "consume_token": activation.consume_token,
                    "adapter_kind": "explicit_activation",
                    "replayed": activation.replayed,
                    "state_changed": activation.state_changed,
                }))
            } else {
                None
            };
        let binding = self
            .agent_continuations
            .as_ref()
            .map(|controller| {
                controller.binding_status(&agent_id, &endpoint_id, expected_controller_generation)
            })
            .unwrap_or(crate::agent_wake::AgentHostBindingStatus {
                adapter_registered: false,
                adapter_kind: None,
                production_auto_resume_available: false,
            });
        // Visible capability is the conjunction of durable Endpoint state and
        // a current callable process-local registration.
        bootstrap.endpoint.wake_capable &= binding.adapter_registered
            && bootstrap.endpoint.lifecycle == AgentEndpointLifecycle::Attached;
        let wake_reply = bootstrap
            .wake
            .as_ref()
            .filter(|wake| {
                matches!(
                    wake.state,
                    AgentWakeState::Prepared
                        | AgentWakeState::Delivered
                        | AgentWakeState::DeliveryUnknown
                )
            })
            .map(|wake| {
                json!({
                "wake_id": wake.wake_id,
                "reply_operation_index_min": 0,
                "reply_operation_index_max": 31,
                    "contract": "For each semantically distinct reply in this accepted Wake, call post_conversation_message with this wake_reply_id and a stable per-send reply_operation_index. Exact retry replays one Message; changed reuse fails closed."
                })
            });
        let runtime_wake_capable = bootstrap.endpoint.wake_capable;
        ToolResult::ok(json!({
            "acting_agent": bootstrap.acting_agent,
            "endpoint": bootstrap.endpoint,
            "selected_conversation": bootstrap.selected_conversation,
            "inbox": bootstrap.inbox,
            "wake": bootstrap.wake,
            "host_binding": {
                "adapter_registered": binding.adapter_registered,
                "adapter_kind": binding.adapter_kind,
                "runtime_wake_capable": runtime_wake_capable,
                "production_auto_resume_available": binding.production_auto_resume_available,
                "manual_fallback": !binding.production_auto_resume_available,
            },
            "reply_replay": wake_reply,
            "wake_activation": wake_activation,
            "bootstrap_note": "Durable state remains authoritative. Read the Agent Inbox and relevant Conversation before acting; this bootstrap contains no transcript or Inbox Message body."
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::shared_key_context;
    use std::sync::Arc;

    fn managed_user(user_id: &str) -> AuthContext {
        let mut auth = AuthContext::new(AuthKind::ApiToken);
        auth.user_id = Some(user_id.to_string());
        auth.username = Some(user_id.to_string());
        auth
    }

    fn assert_same_public_failure(foreign: ToolResult, missing: ToolResult, error_kind: &str) {
        assert!(!foreign.success);
        assert!(!missing.success);
        assert_eq!(foreign.error, missing.error);
        assert_eq!(foreign.output, missing.output);
        assert_eq!(foreign.output["error_kind"], error_kind);
        assert_eq!(foreign.output["recovery_kind"], "fix_input");
    }

    #[test]
    fn stale_endpoint_and_wake_fences_require_reconciliation() {
        for error_kind in [
            "endpoint_expired",
            "endpoint_detached",
            "endpoint_not_active",
            "endpoint_generation_stale",
            "wake_endpoint_fence_mismatch",
            "wake_claim_stale",
        ] {
            assert_eq!(
                communication_recovery_kind(error_kind, RecoveryKind::RetrySame),
                RecoveryKind::Reconcile,
                "{error_kind}"
            );
        }

        assert_eq!(
            communication_recovery_kind("wake_consume_token_mismatch", RecoveryKind::RetrySame),
            RecoveryKind::FixInput
        );
    }

    #[test]
    fn durable_principal_ignores_rotating_api_key_identity() {
        let mut first = AuthContext::new(AuthKind::ApiToken);
        first.user_id = Some("user-123".to_string());
        first.api_key_id = Some("token-a".to_string());
        let mut second = first.clone();
        second.api_key_id = Some("token-b".to_string());
        assert_eq!(
            communication_principal(Some(&first)).unwrap(),
            communication_principal(Some(&second)).unwrap()
        );
    }

    #[test]
    fn direct_and_oauth_shared_key_subjects_share_durable_principal() {
        let direct = shared_key_context("same secret");
        let mut oauth = AuthContext::new(AuthKind::OAuth2Token);
        oauth.token_kind = Some("oauth2_shared_key".to_string());
        oauth.shared_key_hash = direct.shared_key_hash.clone();
        assert_eq!(
            communication_principal(Some(&direct)).unwrap(),
            communication_principal(Some(&oauth)).unwrap()
        );
    }

    #[test]
    fn foreign_resources_match_missing_at_tool_result_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::db::Database::open(&temp.path().join("tool-privacy.db")).unwrap());
        let runtime = ToolRuntime::new_for_tests().with_communication_database(db);
        let alice = managed_user("alice");
        let bob = managed_user("bob");

        let created_agent = runtime.create_agent_identity(
            Some(&bob),
            "bob-agent".to_string(),
            "Bob Agent".to_string(),
            Some("private profile".to_string()),
            vec!["privacy".to_string()],
            "bob-agent-create".to_string(),
        );
        assert!(created_agent.success, "{:?}", created_agent.error);
        let bob_agent_id = created_agent.output["agent"]["agent_id"]
            .as_str()
            .unwrap()
            .to_string();
        let missing_agent = format!("wc_dagent_{}", "9".repeat(32));

        let foreign_agent = runtime.update_agent_identity(
            Some(&alice),
            bob_agent_id.clone(),
            1,
            None,
            None,
            Some("probe".to_string()),
            None,
        );
        let missing_agent_result = runtime.update_agent_identity(
            Some(&alice),
            missing_agent,
            1,
            None,
            None,
            Some("probe".to_string()),
            None,
        );
        assert_same_public_failure(foreign_agent, missing_agent_result, "agent_not_found");

        let created_conversation = runtime.create_conversation(
            Some(&bob),
            Some("Bob private room".to_string()),
            vec![bob_agent_id],
            "bob-room-create".to_string(),
        );
        assert!(
            created_conversation.success,
            "{:?}",
            created_conversation.error
        );
        let bob_conversation_id = created_conversation.output["conversation"]["conversation"]
            ["conversation_id"]
            .as_str()
            .unwrap()
            .to_string();
        let missing_conversation = format!("wc_conv_{}", "8".repeat(32));
        let foreign_conversation = runtime.read_conversation(
            Some(&alice),
            bob_conversation_id,
            None,
            None,
            None,
            Some(0),
            Some(10),
        );
        let missing_conversation_result = runtime.read_conversation(
            Some(&alice),
            missing_conversation,
            None,
            None,
            None,
            Some(0),
            Some(10),
        );
        assert_same_public_failure(
            foreign_conversation,
            missing_conversation_result,
            "conversation_not_found",
        );
    }

    #[test]
    fn project_and_runner_credentials_never_become_communication_principals() {
        for kind in [AuthKind::ProjectCredential, AuthKind::AgentToken] {
            let auth = AuthContext::new(kind);
            let error = communication_principal(Some(&auth)).unwrap_err();
            assert_eq!(
                error.output["error_kind"],
                "communication_principal_unavailable"
            );
        }
    }
}
