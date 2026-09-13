use super::sessions;
use super::tool_definition::runtime_tool_is_shell_like;
use super::{RecoveryKind, ToolResult};
use crate::auth::AuthContext;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub(crate) const SESSION_PROJECT_MISMATCH_KIND: &str = "session_project_mismatch";
const SESSION_ATTENTION_MAX_MESSAGES: usize = 3;
const SESSION_ATTENTION_MAX_BODY_BYTES: usize = 3072;
const SESSION_CONTINUITY_RECOVERY_EVENT_LIMIT: usize = 20;
pub(crate) const SESSION_CONTINUITY_RECOVERY_EVENT_BYTES: usize = 48 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct SessionProjectMismatch {
    pub(crate) session_project: String,
    pub(crate) request_project: String,
}

pub(crate) fn unknown_session_result(session_id: &str) -> ToolResult {
    ToolResult::err_with_output(
        format!("unknown_session_id: {}", session_id),
        json!({
            "error_kind": "unknown_session_id",
            "session_id": session_id,
        }),
    )
    .with_recovery(RecoveryKind::FixInput, None)
}

pub(crate) fn session_authority_denied_result(session_id: &str, tool_name: &str) -> ToolResult {
    ToolResult::err_with_output(
        "session_authority_denied",
        json!({
            "error_kind": "session_authority_denied",
            "failure_kind": "session_authority_denied",
            "session_id": session_id,
            "tool_name": tool_name,
            "state_changed": false,
        }),
    )
    .with_recovery(RecoveryKind::UserAction, None)
}

pub(crate) fn session_project_mismatch_result(
    session_id: &str,
    tool_name: &str,
    mismatch: &SessionProjectMismatch,
) -> ToolResult {
    ToolResult::err_with_output(
        format!(
            "session_project_mismatch: session {} is scoped to project {} but {} requested project {}",
            session_id, mismatch.session_project, tool_name, mismatch.request_project
        ),
        json!({
            "error_kind": SESSION_PROJECT_MISMATCH_KIND,
            "failure_kind": SESSION_PROJECT_MISMATCH_KIND,
            "session_id": session_id,
            "tool_name": tool_name,
            "session_project": mismatch.session_project,
            "request_project": mismatch.request_project,
            "command_started": false,
            "state_changed": false,
        }),
    )
    .with_recovery(RecoveryKind::FixInput, None)
}

pub(crate) fn session_guard_denied_result(
    session_id: &str,
    tool_name: &str,
    denial: sessions::SessionGuardDenial,
) -> ToolResult {
    let mut output = json!({
        "error_kind": "session_guard_denied",
        "session_id": session_id,
        "tool_name": tool_name,
        "guard": denial.guard,
        "mode": denial.mode.as_str(),
    });
    if denial.guard == "deny_shell_tools" {
        output["command_started"] = Value::Bool(false);
    }
    ToolResult::err_with_output(
        format!(
            "session_guard_denied: {} blocked by {} session",
            tool_name,
            denial.mode.as_str()
        ),
        output,
    )
    .with_recovery(RecoveryKind::NoAction, None)
}

/// Lifecycle denial for Closed workflow sessions (write/shell/mutation).
pub(crate) fn session_lifecycle_denied_result(
    session_id: &str,
    tool_name: &str,
    denial: sessions::SessionLifecycleDenial,
) -> ToolResult {
    let lifecycle = denial.lifecycle.as_str();
    let error_kind = match denial.lifecycle {
        sessions::SessionLifecycle::Closed => "session_closed",
        sessions::SessionLifecycle::Active => "session_lifecycle_denied",
    };
    let mut output = json!({
        "error_kind": error_kind,
        "session_id": session_id,
        "tool_name": tool_name,
        "lifecycle": lifecycle,
    });
    // Match shell-guard shape so callers can detect "command never started".
    if runtime_tool_is_shell_like(tool_name) {
        output["command_started"] = Value::Bool(false);
    }
    ToolResult::err_with_output(
        format!("{error_kind}: {tool_name} blocked on {lifecycle} session"),
        output,
    )
    .with_recovery(RecoveryKind::NoAction, None)
}

pub(crate) fn session_message_error_result(
    session_id: &str,
    message_id: Option<&str>,
    error: sessions::SessionMessageError,
) -> ToolResult {
    match error {
        sessions::SessionMessageError::UnknownSession => unknown_session_result(session_id),
        sessions::SessionMessageError::UnknownMessage => ToolResult::err_with_output(
            match message_id {
                Some(message_id) => format!("unknown_message_id: {}", message_id),
                None => "unknown_message_id".to_string(),
            },
            json!({
                "error_kind": "unknown_message_id",
                "session_id": session_id,
                "message_id": message_id,
            }),
        ),
        sessions::SessionMessageError::MessageNotOpen => ToolResult::err_with_output(
            "session_message_not_open",
            json!({
                "error_kind": "session_message_not_open",
                "session_id": session_id,
                "message_id": message_id,
                "state_changed": false,
            }),
        )
        .with_recovery(RecoveryKind::NoAction, None),
        sessions::SessionMessageError::NotTodo => ToolResult::err_with_output(
            "session_message_not_todo",
            json!({
                "error_kind": "session_message_not_todo",
                "session_id": session_id,
                "message_id": message_id,
                "state_changed": false,
            }),
        ),
        sessions::SessionMessageError::IdempotencyConflict => ToolResult::err_with_output(
            "idempotency_conflict",
            json!({
                "error_kind": "idempotency_conflict",
                "session_id": session_id,
                "message_id": message_id,
                "state_changed": false,
            }),
        ),
        sessions::SessionMessageError::AlreadyCompleted {
            answer_message_id,
            completion_id,
        } => ToolResult::err_with_output(
            "already_completed",
            json!({
                "error_kind": "already_completed",
                "session_id": session_id,
                "message_id": message_id,
                "answer_message_id": answer_message_id,
                "completion_id": completion_id,
                "state_changed": false,
            }),
        ),
        sessions::SessionMessageError::InvalidCompletionState => ToolResult::err_with_output(
            "invalid_completion_state",
            json!({
                "error_kind": "invalid_completion_state",
                "session_id": session_id,
                "message_id": message_id,
                "state_changed": false,
            }),
        ),
        sessions::SessionMessageError::InvalidObservationState => ToolResult::err_with_output(
            "invalid_message_observation_state",
            json!({
                "error_kind": "invalid_message_observation_state",
                "session_id": session_id,
                "message_id": message_id,
                "state_changed": false,
            }),
        )
        .with_recovery(RecoveryKind::NoAction, None),
        sessions::SessionMessageError::InvalidAssignmentFence => ToolResult::err_with_output(
            "invalid_assignment_fence",
            json!({
                "error_kind": "invalid_assignment_fence",
                "failure_kind": "invalid_arguments",
                "session_id": session_id,
                "message_id": message_id,
                "state_changed": false,
                "retry_guidance": "read the exact assignment with get_session_assignment and pass its opaque assignment_fence unchanged",
            }),
        )
        .with_recovery(RecoveryKind::FixInput, None),
        sessions::SessionMessageError::AssignmentStale {
            current,
            fresh_assignment_fence,
        } => ToolResult::err_with_output(
            "assignment_stale",
            json!({
                "error_kind": "assignment_stale",
                "failure_kind": "conflict",
                "session_id": session_id,
                "message_id": message_id,
                "state_changed": false,
                "current_assignment": current,
                "fresh_assignment_fence": fresh_assignment_fence,
                "retry_guidance": "re-evaluate the returned current assignment; when fresh_assignment_fence is present it is the durable fence for exactly that returned state, otherwise call get_session_assignment again",
            }),
        )
        .with_recovery(RecoveryKind::Reobserve, None),
        sessions::SessionMessageError::AssignmentHistoryLost { current } => ToolResult::err_with_output(
            "assignment_history_lost",
            json!({
                "error_kind": "assignment_history_lost",
                "failure_kind": "history_lost",
                "session_id": session_id,
                "message_id": message_id,
                "state_changed": false,
                "current_assignment": current,
                "retry_guidance": "retained state cannot prove the full exact assignment; do not complete this todo from stale context",
            }),
        )
        .with_recovery(RecoveryKind::NoAction, None),
        sessions::SessionMessageError::AssignmentTooLarge {
            reply_count,
            max_replies,
            current,
        } => ToolResult::err_with_output(
            "assignment_too_large",
            json!({
                "error_kind": "assignment_too_large",
                "failure_kind": "bounded_output_exceeded",
                "session_id": session_id,
                "message_id": message_id,
                "state_changed": false,
                "reply_count": reply_count,
                "max_replies": max_replies,
                "current_assignment": current,
                "retry_guidance": "the coordinator must consolidate or supersede this assignment before a fenced completion can be issued",
            }),
        )
        .with_recovery(RecoveryKind::NoAction, None),
        sessions::SessionMessageError::PersistenceUncertain => ToolResult::err_with_output(
            "completion_persistence_uncertain",
            json!({
                "error_kind": "completion_persistence_uncertain",
                "failure_kind": "outcome_unknown",
                "session_id": session_id,
                "message_id": message_id,
                "state_changed": true,
                "retry_same_completion": true,
            }),
        )
        .with_recovery(RecoveryKind::RetrySame, None),
        sessions::SessionMessageError::SessionClosed { lifecycle } => ToolResult::err_with_output(
            "session_closed: session message mutation blocked",
            json!({
                "error_kind": "session_closed",
                "session_id": session_id,
                "lifecycle": lifecycle.as_str(),
            }),
        )
        .with_recovery(RecoveryKind::NoAction, None),
        sessions::SessionMessageError::InvalidInput(message) => ToolResult::err_with_output(
            message.clone(),
            json!({
                "error_kind": "invalid_session_message",
                "session_id": session_id,
                "error": message,
            }),
        )
        .with_recovery(RecoveryKind::FixInput, None),
    }
}

#[cfg(test)]
#[test]
fn completion_persistence_uncertain_exposes_exact_retry_same_recovery() {
    let result = session_message_error_result(
        "wc_sess_test",
        Some("wc_msg_test"),
        sessions::SessionMessageError::PersistenceUncertain,
    );
    assert!(!result.success);
    assert_eq!(
        result.output["error_kind"],
        "completion_persistence_uncertain"
    );
    assert_eq!(result.output["failure_kind"], "outcome_unknown");
    assert_eq!(result.output["state_changed"], true);
    assert_eq!(result.output["retry_same_completion"], true);
    assert_eq!(result.output["recovery_kind"], "retry_same");
    assert!(result.output.get("recovery_tool").is_none());
}

pub(crate) fn add_session_hint(
    result: &mut ToolResult,
    sessions: &sessions::SessionStore,
    session_id: &str,
) {
    let hint = sessions.inbox_hint(session_id);
    if let Some(output) = result.output.as_object_mut() {
        // The outer recorder is authoritative only for collaboration guidance.
        // Recorder provenance stays ledger-only, and any business `session_id`
        // produced by the concrete tool is preserved untouched.
        output.remove("session_hint");
        if let Some(hint) = hint {
            output.insert(
                "session_hint".to_string(),
                serde_json::to_value(hint).unwrap_or(Value::Null),
            );
        }
        return;
    }
    if let Some(hint) = hint {
        let prior = std::mem::take(&mut result.output);
        result.output = json!({
            "value": prior,
            "session_hint": hint,
        });
    }
}

fn model_facing_recovery_event(event: &sessions::SessionEvent) -> Value {
    json!({
        "context_revision": event.context_revision,
        "tool_name": event.tool_name,
        "status": event.status,
        "changed_paths": event.changed_paths,
        "job_id": event.job_id,
        "error_kind": event.error_kind,
        "effect_evidence": event.effect_evidence,
        "context_result": event.context_result_summary,
        "execution_summary": event.validation_output_summary,
    })
}

fn bounded_model_facing_recovery_events(
    recorded: &sessions::RecordedModelFacingToolCall,
) -> Vec<Value> {
    let mut events = Vec::new();
    for event in recorded
        .recovery_events
        .iter()
        .rev()
        .take(SESSION_CONTINUITY_RECOVERY_EVENT_LIMIT)
    {
        events.insert(0, model_facing_recovery_event(event));
        let fits = serde_json::to_vec(&events)
            .map(|bytes| bytes.len() <= SESSION_CONTINUITY_RECOVERY_EVENT_BYTES)
            .unwrap_or(false);
        if !fits {
            events.remove(0);
            break;
        }
    }
    events
}

/// A hint is sufficient to request recovery, never to certify model knowledge.
fn require_context_recovery(output: &mut Value, status: &str, session_id: &str) {
    output
        .as_object_mut()
        .unwrap()
        .remove("session_context_revision");
    output["session_continuity"] = json!({
        "status": status,
        "recovery_required": true,
        "recovery_tool": "session_handoff_summary",
        "recovery_session_id": session_id,
    });
}

/// Only the canonical handoff dispatcher calls this, after authorized execution.
/// The revision is captured before reading any state; all requested components
/// must be present and no checkpoint may complete across that observation.
pub(crate) fn establish_handoff_context_baseline(
    result: &mut ToolResult,
    session_id: &str,
    observed_revision: Option<u64>,
    current_revision: Option<u64>,
) {
    if result.success && observed_revision.is_some() && observed_revision == current_revision {
        result.output["session_context_revision"] = json!(observed_revision.unwrap());
        result.output["session_continuity"] = json!({"status": "recovered"});
    } else {
        require_context_recovery(&mut result.output, "unacknowledged", session_id);
    }
}

pub(crate) fn add_session_context_continuity(
    result: &mut ToolResult,
    recorded: &sessions::RecordedModelFacingToolCall,
) -> bool {
    debug_assert_eq!(
        recorded.checkpoint_advanced,
        recorded.context_revision > recorded.pre_response_context_revision,
        "checkpoint allocation must match the recorded pre-response watermark"
    );
    if matches!(
        recorded.ack_session_context_revision,
        sessions::SessionContextRevisionAck::Unsupported
    ) {
        return false;
    }
    // Only a supported non-checkpoint carrier can supply this trusted business
    // projection. Fence the exact recorder and the completion watermark again:
    // a different Session's handoff or a concurrent completion proves no baseline.
    if !recorded.checkpoint_advanced
        && result
            .output
            .pointer("/session_continuity/status")
            .and_then(Value::as_str)
            == Some("recovered")
    {
        if result.success
            && result.output.get("session_id").and_then(Value::as_str)
                == Some(recorded.session_id.as_str())
            && result
                .output
                .get("session_context_revision")
                .and_then(Value::as_u64)
                == Some(recorded.context_revision)
        {
            return false;
        }
        require_context_recovery(&mut result.output, "unacknowledged", &recorded.session_id);
        return true;
    }
    let pre_response_context_revision = recorded.pre_response_context_revision;
    let (status, ack_revision, needs_recovery, events_after_ack) = match recorded
        .ack_session_context_revision
    {
        // An ACK can only prove state that already existed when the request
        // started. A numerically matching revision allocated by a concurrent
        // completion after request admission is still a future/invalid ACK.
        sessions::SessionContextRevisionAck::Revision(revision)
            if revision > recorded.pre_call_context_revision =>
        {
            ("invalid", Some(revision), true, None)
        }
        sessions::SessionContextRevisionAck::Revision(revision)
            if revision == pre_response_context_revision =>
        {
            ("exact", Some(revision), false, None)
        }
        sessions::SessionContextRevisionAck::Revision(revision) => (
            "behind",
            Some(revision),
            true,
            Some(pre_response_context_revision.saturating_sub(revision)),
        ),
        sessions::SessionContextRevisionAck::Unsupported => {
            unreachable!("unsupported continuity requests return before response decoration")
        }
        sessions::SessionContextRevisionAck::Unacknowledged => ("unacknowledged", None, true, None),
        sessions::SessionContextRevisionAck::Invalid => ("invalid", None, true, None),
    };
    if !needs_recovery && !recorded.checkpoint_advanced {
        return false;
    }
    let mut output = match std::mem::take(&mut result.output) {
        Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("value".to_string(), other);
            map
        }
    };
    output.insert(
        "session_context_revision".to_string(),
        Value::from(recorded.context_revision),
    );
    if !needs_recovery {
        result.output = Value::Object(output);
        return false;
    }

    if status != "behind" {
        result.output = Value::Object(output);
        require_context_recovery(&mut result.output, status, &recorded.session_id);
        return true;
    }

    let total_retained = recorded.recovery_events.len();
    let events = bounded_model_facing_recovery_events(recorded);
    let omitted_count = total_retained.saturating_sub(events.len());
    let mut continuity = json!({
        "status": status,
        "ack_revision": ack_revision,
        "pre_call_revision": recorded.pre_call_context_revision,
        "history_lost": recorded.history_lost,
    });
    if let Some(events_after_ack) = events_after_ack {
        continuity["events_after_ack"] = Value::from(events_after_ack);
    }
    output.insert("session_continuity".to_string(), continuity);
    output.insert(
        "session_recovery".to_string(),
        json!({
            "model_facing_events": events,
            "omitted_count": omitted_count,
            "truncated": omitted_count > 0,
            "history_lost": recorded.history_lost,
        }),
    );
    if recorded.history_lost || omitted_count > 0 {
        output.remove("session_context_revision");
        let continuity = output.get_mut("session_continuity").unwrap();
        continuity["recovery_required"] = json!(true);
        continuity["recovery_tool"] = json!("session_handoff_summary");
        continuity["recovery_session_id"] = json!(recorded.session_id);
    }
    result.output = Value::Object(output);
    true
}

pub(crate) fn observe_session_attention_acks(
    sessions: &sessions::SessionStore,
    session_id: &str,
    ack_message_ids: &[String],
) -> sessions::SessionAckObservation {
    sessions.observe_message_acks(session_id, ack_message_ids)
}

pub(crate) fn add_session_attention_projection(
    result: &mut ToolResult,
    sessions: &sessions::SessionStore,
    session_id: &str,
    ack: &sessions::SessionAckObservation,
    ack_requested: bool,
) {
    let attention = sessions.ack_required_guidance(session_id, &ack.accepted_ids);
    let unsuppressed_count = attention.messages.len();
    let mut remaining_bytes = SESSION_ATTENTION_MAX_BODY_BYTES;
    let mut messages = Vec::new();
    let mut body_truncated = false;
    for message in attention
        .messages
        .into_iter()
        .take(SESSION_ATTENTION_MAX_MESSAGES)
    {
        if remaining_bytes == 0 {
            break;
        }
        let (body, truncated) = bound_utf8_bytes(&message.message, remaining_bytes);
        body_truncated |= truncated;
        remaining_bytes = remaining_bytes.saturating_sub(body.len());
        messages.push(json!({
            "message_id": message.message_id,
            "kind": message.kind.as_str(),
            "priority": message.priority,
            "created_at": message.created_at,
            "message": body,
            "message_truncated": truncated,
        }));
    }
    if attention.total_open_requires_ack == 0 && !ack_requested {
        return;
    }
    let omitted_count = unsuppressed_count.saturating_sub(messages.len());
    let mut output = match std::mem::take(&mut result.output) {
        Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("value".to_string(), other);
            map
        }
    };
    // Strong hint fields are a counts-only fallback. Once this response has
    // fully conveyed every unacknowledged urgent body (or the request ACK has
    // suppressed it), keep only the ordinary inbox counts/tool suggestion.
    // Preserve the strong fallback when any urgent body is omitted or truncated.
    if omitted_count == 0 && !body_truncated {
        if let Some(hint) = output
            .get_mut("session_hint")
            .and_then(Value::as_object_mut)
        {
            hint.remove("attention_required");
            hint.remove("attention_reason");
            hint.remove("attention_instruction");
        }
    }
    output.insert(
        "session_attention".to_string(),
        json!({
            "requires_ack": attention.total_open_requires_ack > 0,
            "messages": messages,
            "omitted_count": omitted_count,
            "truncated": omitted_count > 0,
            "ack": {
                "accepted_count": ack.accepted_count,
                "ignored_count": ack.ignored_count,
            }
        }),
    );
    result.output = Value::Object(output);
}

#[cfg(test)]
pub(crate) fn add_session_attention(
    result: &mut ToolResult,
    sessions: &sessions::SessionStore,
    session_id: &str,
    ack_message_ids: &[String],
) {
    let ack = observe_session_attention_acks(sessions, session_id, ack_message_ids);
    add_session_attention_projection(
        result,
        sessions,
        session_id,
        &ack,
        !ack_message_ids.is_empty(),
    );
}

fn bound_utf8_bytes(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_string(), false);
    }
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_string(), true)
}

pub(crate) fn workflow_session_authority_fingerprint(
    auth: Option<&AuthContext>,
) -> Result<String, String> {
    let (authority_kind, authority_id) = match auth {
        None => ("local-dev", "local-dev".to_string()),
        Some(auth) if auth.is_bootstrap => ("bootstrap", "server-bootstrap".to_string()),
        Some(auth) if auth.is_oauth_shared_key_subject() || auth.is_shared_key() => (
            "shared-key-group",
            stable_workflow_authority_id(
                auth.shared_key_hash.as_deref(),
                "shared-key authority has no stable group identity",
            )?,
        ),
        Some(auth)
            if auth.is_oauth_project_subject()
                || auth.is_project_credential()
                || auth.is_agent_token() =>
        {
            (
                "project-grant",
                stable_workflow_authority_id(
                    auth.project_grant_id.as_deref(),
                    "project-grant authority has no stable grant identity",
                )?,
            )
        }
        Some(auth) if auth.is_open_anonymous() => {
            // Open mode deliberately has one shared authority group. It remains
            // unsuitable for owning a project-less Session, which is rejected at
            // Session creation where there is no project boundary to contain it.
            ("open-anonymous", "open-anonymous".to_string())
        }
        Some(auth)
            if matches!(
                auth.kind,
                crate::auth::AuthKind::ApiToken
                    | crate::auth::AuthKind::AccountCredential
                    | crate::auth::AuthKind::OAuth2Token
            ) =>
        {
            (
                "managed-user",
                stable_workflow_authority_id(
                    auth.user_id.as_deref(),
                    "managed caller has no stable user identity",
                )?,
            )
        }
        Some(_) => {
            return Err(
                "authenticated caller has no canonical Workflow Session authority identity"
                    .to_string(),
            );
        }
    };
    Ok(hash_workflow_session_authority(
        b"webcodex.workflow-session-authority.v1\0",
        authority_kind,
        &authority_id,
    ))
}

fn stable_workflow_authority_id(value: Option<&str>, error: &str) -> Result<String, String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| error.to_string())
}

fn hash_workflow_session_authority(domain: &[u8], kind: &str, id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(id.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub(crate) fn canonical_repository_key(repository_root: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex.workflow-repository-root.v1\0");
    hasher.update(repository_root.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub(crate) fn runtime_observation_principal(
    auth: Option<&AuthContext>,
) -> Result<(String, String), String> {
    let Some(auth) = auth else {
        return Ok(("dev".to_string(), "dev".to_string()));
    };
    if auth.is_bootstrap {
        return Ok((
            "bootstrap".to_string(),
            auth.user_id
                .as_deref()
                .or(auth.username.as_deref())
                .unwrap_or("bootstrap")
                .to_string(),
        ));
    }
    let id = if matches!(auth.kind, crate::auth::AuthKind::OpenAnonymous) {
        Some("open-anonymous".to_string())
    } else {
        auth.api_key_id
            .as_deref()
            .or(auth.user_id.as_deref())
            .or(auth.username.as_deref())
            .or(auth.allowed_client_id.as_deref())
            .or(auth.shared_key_hash.as_deref())
            .or(auth.project_grant_id.as_deref())
            .map(str::to_string)
    };
    let Some(principal_id) = id else {
        return Err(
            "runtime_observation_unavailable: authenticated caller has no stable principal id"
                .to_string(),
        );
    };
    let principal_kind = auth.principal_kind();
    Ok((principal_kind.to_string(), principal_id))
}
