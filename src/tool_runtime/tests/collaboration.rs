//! Workflow Session collaboration tests: coordinator/worker isolation, provenance, and authority.

use super::super::kernel::{
    HostFileImportTrust, ToolCallContext, ToolCallRequest, ToolInvocationMetadata,
    ToolProtocolCapabilities, ToolTransport,
};
use super::super::sessions::{
    self, PostSessionMessageInput, SessionMessageKind, SessionMessagePriority,
};
use super::super::ToolRuntime;
use super::support::*;
use crate::auth::AuthContext;
use crate::client_window::ClientWindow;
use crate::runner_protocol::RunnerCapabilities;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

async fn call_with_recorder(
    runtime: &ToolRuntime,
    tool_name: &str,
    arguments: Value,
    recorder_session_id: Option<&str>,
    auth: &AuthContext,
    window: Option<&ClientWindow>,
) -> super::super::ToolResult {
    call_with_recorder_metadata(
        runtime,
        tool_name,
        arguments,
        recorder_session_id,
        ToolInvocationMetadata::default(),
        auth,
        window,
    )
    .await
}

async fn call_with_recorder_metadata(
    runtime: &ToolRuntime,
    tool_name: &str,
    arguments: Value,
    recorder_session_id: Option<&str>,
    invocation_metadata: ToolInvocationMetadata,
    auth: &AuthContext,
    window: Option<&ClientWindow>,
) -> super::super::ToolResult {
    let outcome = runtime
        .call_tool_with_invocation_metadata(
            ToolCallRequest {
                tool_name: tool_name.to_string(),
                arguments,
            },
            ToolCallContext {
                transport: ToolTransport::Api,
                session_id: recorder_session_id,
                auth: Some(auth),
                window,
                record_oauth_scope_denials: false,
                host_file_import_trust: HostFileImportTrust::Untrusted,
            },
            invocation_metadata,
            ToolProtocolCapabilities::default(),
        )
        .await;
    assert!(
        outcome.error_status.is_none(),
        "unexpected transport error: {:?}",
        outcome.error_status
    );
    outcome.result.expect("tool result")
}

fn tool_names(runtime: &ToolRuntime, session_id: &str) -> Vec<String> {
    runtime
        .sessions
        .summary(session_id, Some(100))
        .unwrap()
        .events
        .iter()
        .filter(|event| event.kind == "tool_call_finished")
        .map(|event| event.tool_name.clone())
        .collect()
}

fn start_authorized_project_session(
    runtime: &ToolRuntime,
    project: &str,
    title: Option<&str>,
    auth: &AuthContext,
) -> sessions::SessionSummary {
    let fingerprint =
        super::super::session_context::workflow_session_authority_fingerprint(Some(auth))
            .expect("test authority must have a stable identity");
    runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(project.to_string()),
                title.map(str::to_string),
                super::super::SessionMode::Normal,
                sessions::SessionGuards::default(),
            )
            .with_owner_authority_fingerprint(Some(fingerprint)),
        )
        .unwrap()
}

fn start_authorized_unscoped_session(
    runtime: &ToolRuntime,
    title: &str,
    auth: &AuthContext,
) -> sessions::SessionSummary {
    let fingerprint =
        super::super::session_context::workflow_session_authority_fingerprint(Some(auth))
            .expect("test authority must have a stable identity");
    runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                None,
                Some(title.to_string()),
                super::super::SessionMode::Normal,
                sessions::SessionGuards::default(),
            )
            .with_owner_authority_fingerprint(Some(fingerprint)),
        )
        .unwrap()
}

#[tokio::test]
async fn request_scoped_ack_suppresses_only_current_response_and_records_first_observation_once() {
    let runtime = test_runtime();
    let auth = auth_context(None, true);
    let session = start_authorized_unscoped_session(&runtime, "ack recorder", &auth);
    let foreign = start_authorized_unscoped_session(&runtime, "foreign ack", &auth);
    let guidance = runtime
        .sessions
        .post_message_with_ack(
            PostSessionMessageInput {
                session_id: session.session_id.clone(),
                kind: SessionMessageKind::Guidance,
                message: "Keep the compatibility fence intact.".to_string(),
                tags: Vec::new(),
                reply_to: None,
                priority: SessionMessagePriority::High,
            },
            true,
        )
        .unwrap();
    let foreign_guidance = runtime
        .sessions
        .post_message_with_ack(
            PostSessionMessageInput {
                session_id: foreign.session_id.clone(),
                kind: SessionMessageKind::Guidance,
                message: "foreign secret guidance".to_string(),
                tags: Vec::new(),
                reply_to: None,
                priority: SessionMessagePriority::High,
            },
            true,
        )
        .unwrap();

    let first = call_with_recorder(
        &runtime,
        "list_tools",
        json!({}),
        Some(&session.session_id),
        &auth,
        None,
    )
    .await;
    assert!(first.success, "{:?}", first.error);
    assert_eq!(
        first.output["session_attention"]["messages"][0]["message_id"],
        guidance.message_id
    );
    assert_eq!(
        first.output["session_attention"]["messages"][0]["message"],
        "Keep the compatibility fence intact."
    );
    assert!(first.output["session_hint"]
        .get("attention_required")
        .is_none());
    assert!(first.output["session_hint"]
        .get("attention_instruction")
        .is_none());

    let acknowledged = call_with_recorder_metadata(
        &runtime,
        "list_tools",
        json!({}),
        Some(&session.session_id),
        ToolInvocationMetadata {
            ack_session_message_ids: vec![guidance.message_id.clone()],
            ..Default::default()
        },
        &auth,
        None,
    )
    .await;
    assert!(acknowledged.success, "{:?}", acknowledged.error);
    assert_eq!(
        acknowledged.output["session_attention"]["ack"]["accepted_count"],
        1
    );
    assert!(acknowledged.output["session_attention"]["messages"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(acknowledged.output["session_hint"]
        .get("attention_required")
        .is_none());
    assert!(acknowledged.output["session_hint"]
        .get("attention_instruction")
        .is_none());
    let stored = runtime
        .sessions
        .list_messages(
            &session.session_id,
            sessions::ListSessionMessagesFilter {
                message_id: Some(guidance.message_id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(stored[0].status, sessions::SessionMessageStatus::Open);
    let first_ack_at = stored[0]
        .first_ack_observed_at
        .expect("first ACK timestamp");
    let after_first_ack = runtime
        .sessions
        .observe_messages(&session.session_id, None, None, None)
        .await
        .unwrap();

    let repeated = call_with_recorder_metadata(
        &runtime,
        "list_tools",
        json!({}),
        Some(&session.session_id),
        ToolInvocationMetadata {
            ack_session_message_ids: vec![guidance.message_id.clone()],
            ..Default::default()
        },
        &auth,
        None,
    )
    .await;
    assert!(repeated.success);
    let after_repeat = runtime
        .sessions
        .observe_messages(
            &session.session_id,
            Some(&after_first_ack.observation_token),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(
        !after_repeat.changed,
        "repeated ACK must not churn observation revision"
    );
    let stored_after_repeat = runtime
        .sessions
        .list_messages(
            &session.session_id,
            sessions::ListSessionMessagesFilter {
                message_id: Some(guidance.message_id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        stored_after_repeat[0].first_ack_observed_at,
        Some(first_ack_at)
    );

    let forgotten = call_with_recorder(
        &runtime,
        "list_tools",
        json!({}),
        Some(&session.session_id),
        &auth,
        None,
    )
    .await;
    assert!(forgotten.success, "business tool must still execute");
    assert_eq!(
        forgotten.output["session_attention"]["messages"][0]["message_id"],
        guidance.message_id
    );

    let foreign_ack = call_with_recorder_metadata(
        &runtime,
        "list_tools",
        json!({}),
        Some(&session.session_id),
        ToolInvocationMetadata {
            ack_session_message_ids: vec![
                foreign_guidance.message_id.clone(),
                "wc_msg_unknown".to_string(),
            ],
            ..Default::default()
        },
        &auth,
        None,
    )
    .await;
    assert!(foreign_ack.success, "ignored ACK must not block the tool");
    assert_eq!(
        foreign_ack.output["session_attention"]["ack"]["accepted_count"],
        0
    );
    assert_eq!(
        foreign_ack.output["session_attention"]["ack"]["ignored_count"],
        2
    );
    assert_eq!(
        foreign_ack.output["session_attention"]["messages"][0]["message_id"],
        guidance.message_id
    );
    let foreign_stored = runtime
        .sessions
        .list_messages(
            &foreign.session_id,
            sessions::ListSessionMessagesFilter {
                message_id: Some(foreign_guidance.message_id),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(foreign_stored[0].first_ack_observed_at.is_none());

    runtime
        .sessions
        .resolve_message(
            &session.session_id,
            &guidance.message_id,
            Some("handled".to_string()),
        )
        .unwrap();
    let resolved = call_with_recorder(
        &runtime,
        "list_tools",
        json!({}),
        Some(&session.session_id),
        &auth,
        None,
    )
    .await;
    assert!(resolved.success);
    assert!(resolved.output.get("session_attention").is_none());
}

#[tokio::test]
async fn outer_recorder_attention_never_projects_cross_session_business_target_attention() {
    let runtime = test_runtime();
    let auth = auth_context(None, true);
    let recorder = start_authorized_unscoped_session(&runtime, "attention recorder", &auth);
    let target = start_authorized_unscoped_session(&runtime, "business target", &auth);
    let recorder_guidance = runtime
        .sessions
        .post_message_with_ack(
            PostSessionMessageInput {
                session_id: recorder.session_id.clone(),
                kind: SessionMessageKind::Guidance,
                message: "recorder guidance".to_string(),
                tags: Vec::new(),
                reply_to: None,
                priority: SessionMessagePriority::High,
            },
            true,
        )
        .unwrap();
    let target_guidance = runtime
        .sessions
        .post_message_with_ack(
            PostSessionMessageInput {
                session_id: target.session_id.clone(),
                kind: SessionMessageKind::Guidance,
                message: "target guidance".to_string(),
                tags: Vec::new(),
                reply_to: None,
                priority: SessionMessagePriority::High,
            },
            true,
        )
        .unwrap();

    let result = call_with_recorder(
        &runtime,
        "list_session_messages",
        json!({"session_id": target.session_id}),
        Some(&recorder.session_id),
        &auth,
        None,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    let attention_ids: Vec<&str> = result.output["session_attention"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["message_id"].as_str())
        .collect();
    assert_eq!(attention_ids, vec![recorder_guidance.message_id.as_str()]);
    assert!(!attention_ids.contains(&target_guidance.message_id.as_str()));
}

#[tokio::test]
async fn ack_and_resolve_same_outer_request_observes_ack_before_business_mutation() {
    let runtime = test_runtime();
    let auth = auth_context(None, true);
    let session = start_authorized_unscoped_session(&runtime, "ack resolve", &auth);
    let guidance = runtime
        .sessions
        .post_message_with_ack(
            PostSessionMessageInput {
                session_id: session.session_id.clone(),
                kind: SessionMessageKind::Guidance,
                message: "ack before resolving".to_string(),
                tags: Vec::new(),
                reply_to: None,
                priority: SessionMessagePriority::High,
            },
            true,
        )
        .unwrap();

    let result = call_with_recorder_metadata(
        &runtime,
        "resolve_session_message",
        json!({
            "session_id": session.session_id,
            "message_id": guidance.message_id,
            "resolution": "handled"
        }),
        Some(&session.session_id),
        ToolInvocationMetadata {
            ack_session_message_ids: vec![guidance.message_id.clone()],
            ..Default::default()
        },
        &auth,
        None,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["message"]["status"], "resolved");
    assert_eq!(result.output["session_attention"]["requires_ack"], false);
    assert_eq!(
        result.output["session_attention"]["ack"]["accepted_count"],
        1
    );
    assert_eq!(
        result.output["session_attention"]["ack"]["ignored_count"],
        0
    );
    assert!(result.output["session_attention"]["messages"]
        .as_array()
        .unwrap()
        .is_empty());
    let stored = runtime
        .sessions
        .list_messages(
            &session.session_id,
            sessions::ListSessionMessagesFilter {
                message_id: Some(guidance.message_id),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(stored[0].status, sessions::SessionMessageStatus::Resolved);
    assert!(stored[0].first_ack_observed_at.is_some());
}

#[test]
fn urgent_guidance_attention_is_bounded_safe_and_also_decorates_failure_results() {
    let runtime = test_runtime();
    let session = runtime
        .sessions
        .start_session(None, Some("attention bounds".to_string()));
    for index in 0..5 {
        runtime
            .sessions
            .post_message_with_ack(
                PostSessionMessageInput {
                    session_id: session.session_id.clone(),
                    kind: SessionMessageKind::Guidance,
                    message: format!("guidance-{index}-{}", "x".repeat(1800)),
                    tags: vec!["must-not-piggyback".to_string()],
                    reply_to: None,
                    priority: SessionMessagePriority::High,
                },
                true,
            )
            .unwrap();
    }
    let mut failed = super::super::ToolResult::err_with_output(
        "synthetic failure",
        json!({"error_kind": "synthetic"}),
    );
    super::super::add_session_hint(&mut failed, &runtime.sessions, &session.session_id);
    assert_eq!(failed.output["session_hint"]["attention_required"], true);
    super::super::session_context::add_session_attention(
        &mut failed,
        &runtime.sessions,
        &session.session_id,
        &[],
    );
    assert!(!failed.success);
    assert_eq!(failed.output["error_kind"], "synthetic");
    let attention = &failed.output["session_attention"];
    assert_eq!(attention["requires_ack"], true);
    assert_eq!(attention["messages"].as_array().unwrap().len(), 2);
    assert_eq!(attention["omitted_count"], 3);
    assert_eq!(attention["truncated"], true);
    assert_eq!(failed.output["session_hint"]["attention_required"], true);
    assert_eq!(
        failed.output["session_hint"]["attention_reason"],
        "high_priority_guidance_requires_ack"
    );
    let body_bytes: usize = attention["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["message"].as_str().unwrap().len())
        .sum();
    assert!(body_bytes <= 3072);
    let serialized = serde_json::to_string(attention).unwrap();
    assert!(!serialized.contains("must-not-piggyback"));
    for forbidden in [
        "tags",
        "completion_id",
        "completion_key",
        "tool_arguments",
        "credentials",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "leaked {forbidden}: {serialized}"
        );
    }
}

#[tokio::test]
async fn observe_session_messages_collaboration_recorder_target_scope_fences() {
    let runtime = runtime_with_resolver_projects().await;
    let auth = auth_context(None, true);
    let coordinator = start_authorized_project_session(
        &runtime,
        "agent:workstation:my-repo",
        Some("observation coordinator"),
        &auth,
    );
    let same_project_worker = start_authorized_project_session(
        &runtime,
        "agent:workstation:my-repo",
        Some("same-project worker"),
        &auth,
    );
    let other_project_worker = start_authorized_project_session(
        &runtime,
        "agent:workstation:other-repo",
        Some("other-project worker"),
        &auth,
    );

    let allowed = call_with_recorder(
        &runtime,
        "observe_session_messages",
        json!({"session_id": coordinator.session_id}),
        Some(&same_project_worker.session_id),
        &auth,
        None,
    )
    .await;
    assert!(allowed.success, "{:?}", allowed.error);
    assert!(allowed.output["messages"].as_array().unwrap().is_empty());
    assert!(allowed.output["observation_token"].as_str().is_some());
    let baseline_token = allowed.output["observation_token"]
        .as_str()
        .unwrap()
        .to_string();
    let wait_without_token = call_with_recorder(
        &runtime,
        "observe_session_messages",
        json!({"session_id": coordinator.session_id, "wait_secs": 1}),
        Some(&same_project_worker.session_id),
        &auth,
        None,
    )
    .await;
    assert!(!wait_without_token.success);
    assert_eq!(
        wait_without_token.output["error_kind"],
        "invalid_session_message_observation_request"
    );
    assert_eq!(
        wait_without_token.output["failure_kind"],
        "invalid_arguments"
    );
    assert_eq!(wait_without_token.output["state_changed"], false);
    assert_eq!(wait_without_token.output["recovery_kind"], "fix_input");
    assert!(wait_without_token.output.get("recovery_tool").is_none());

    let invalid_token = call_with_recorder(
        &runtime,
        "observe_session_messages",
        json!({
            "session_id": coordinator.session_id,
            "after_observation_token": "not-a-token"
        }),
        Some(&same_project_worker.session_id),
        &auth,
        None,
    )
    .await;
    assert!(!invalid_token.success);
    assert_eq!(
        invalid_token.output["error_kind"],
        "invalid_session_message_observation_token"
    );
    assert_eq!(invalid_token.output["failure_kind"], "invalid_arguments");
    assert_eq!(invalid_token.output["state_changed"], false);
    assert_eq!(invalid_token.output["recovery_kind"], "fix_input");
    assert!(invalid_token.output.get("recovery_tool").is_none());
    assert!(invalid_token.output.get("observation_token").is_none());

    let cross_project = call_with_recorder(
        &runtime,
        "observe_session_messages",
        json!({"session_id": coordinator.session_id}),
        Some(&other_project_worker.session_id),
        &auth,
        None,
    )
    .await;
    assert!(!cross_project.success);
    assert_eq!(
        cross_project.output["error_kind"],
        "session_project_mismatch"
    );
    assert!(cross_project.output.get("observation_token").is_none());

    let unscoped_worker = call_with_recorder(
        &runtime,
        "start_session",
        json!({"title": "unscoped observation worker"}),
        None,
        &auth,
        None,
    )
    .await;
    assert!(unscoped_worker.success, "{:?}", unscoped_worker.error);
    let unscoped_worker_id = unscoped_worker.output["session_id"].as_str().unwrap();
    let unscoped_to_scoped = call_with_recorder(
        &runtime,
        "observe_session_messages",
        json!({"session_id": coordinator.session_id}),
        Some(unscoped_worker_id),
        &auth,
        None,
    )
    .await;
    assert!(!unscoped_to_scoped.success);
    assert_eq!(
        unscoped_to_scoped.output["error_kind"],
        "session_project_mismatch"
    );

    let unscoped_coordinator = call_with_recorder(
        &runtime,
        "start_session",
        json!({"title": "unscoped observation coordinator"}),
        None,
        &auth,
        None,
    )
    .await;
    assert!(
        unscoped_coordinator.success,
        "{:?}",
        unscoped_coordinator.error
    );
    let unscoped_coordinator_id = unscoped_coordinator.output["session_id"].as_str().unwrap();
    let scoped_to_unscoped = call_with_recorder(
        &runtime,
        "observe_session_messages",
        json!({"session_id": unscoped_coordinator_id}),
        Some(&same_project_worker.session_id),
        &auth,
        None,
    )
    .await;
    assert!(!scoped_to_unscoped.success);
    assert_eq!(
        scoped_to_unscoped.output["error_kind"],
        "session_project_mismatch"
    );
    let closed = call_with_recorder(
        &runtime,
        "close_session",
        json!({"session_id": coordinator.session_id}),
        Some(&same_project_worker.session_id),
        &auth,
        None,
    )
    .await;
    assert!(closed.success, "{:?}", closed.error);
    let closed_observation = call_with_recorder(
        &runtime,
        "observe_session_messages",
        json!({
            "session_id": coordinator.session_id,
            "after_observation_token": baseline_token
        }),
        Some(&same_project_worker.session_id),
        &auth,
        None,
    )
    .await;
    assert!(closed_observation.success, "{:?}", closed_observation.error);
    assert_eq!(closed_observation.output["changed"], false);
}

#[tokio::test]
async fn observe_session_messages_collaboration_projectless_owner_and_foreign_recorder_fence() {
    let runtime = test_runtime();
    let alice = shared_key_auth_context("observation-projectless-alice");
    let bob = shared_key_auth_context("observation-projectless-bob");

    let alice_coordinator = call_with_recorder(
        &runtime,
        "start_session",
        json!({"title": "Alice observation coordinator"}),
        None,
        &alice,
        None,
    )
    .await;
    let alice_worker = call_with_recorder(
        &runtime,
        "start_session",
        json!({"title": "Alice observation worker"}),
        None,
        &alice,
        None,
    )
    .await;
    let bob_worker = call_with_recorder(
        &runtime,
        "start_session",
        json!({"title": "Bob observation worker"}),
        None,
        &bob,
        None,
    )
    .await;
    assert!(alice_coordinator.success && alice_worker.success && bob_worker.success);
    let coordinator_id = alice_coordinator.output["session_id"].as_str().unwrap();
    let alice_worker_id = alice_worker.output["session_id"].as_str().unwrap();
    let bob_worker_id = bob_worker.output["session_id"].as_str().unwrap();

    let same_owner = call_with_recorder(
        &runtime,
        "observe_session_messages",
        json!({"session_id": coordinator_id}),
        Some(alice_worker_id),
        &alice,
        None,
    )
    .await;
    assert!(same_owner.success, "{:?}", same_owner.error);

    let foreign_recorder = call_with_recorder(
        &runtime,
        "observe_session_messages",
        json!({"session_id": coordinator_id}),
        Some(bob_worker_id),
        &alice,
        None,
    )
    .await;
    assert!(!foreign_recorder.success);
    assert_eq!(
        foreign_recorder.output["error_kind"],
        "session_authority_denied"
    );
    assert!(foreign_recorder.output.get("observation_token").is_none());

    let foreign_target = call_with_recorder(
        &runtime,
        "observe_session_messages",
        json!({"session_id": coordinator_id}),
        None,
        &bob,
        None,
    )
    .await;
    assert!(!foreign_target.success);
    assert_eq!(
        foreign_target.output["error_kind"],
        "session_authority_denied"
    );
}

#[tokio::test]
async fn collaboration_two_sessions_keep_execution_history_and_explicit_provenance_independent() {
    let client_id = "collaboration-runtime";
    let runtime = runtime_with_agent_project(client_id);
    register_agent(&runtime, client_id, None, RunnerCapabilities::default()).await;
    let project = agent_test_project_id(client_id);
    let auth = auth_context(None, true);
    let coordinator =
        start_authorized_project_session(&runtime, &project, Some("coordinator C"), &auth);
    let worker = start_authorized_project_session(&runtime, &project, Some("worker W"), &auth);
    assert_ne!(coordinator.session_id, worker.session_id);

    let coordinator_window = ClientWindow::for_test("collaboration-coordinator-window");
    let worker_window = ClientWindow::for_test("collaboration-worker-window");
    let posted = call_with_recorder(
        &runtime,
        "post_session_message",
        json!({
            "session_id": coordinator.session_id,
            "kind": "todo",
            "message": "Independent review this exact synthetic change; report findings.",
            "tags": ["review"],
            "priority": "high"
        }),
        Some(&coordinator.session_id),
        &auth,
        Some(&coordinator_window),
    )
    .await;
    assert!(posted.success, "{:?}", posted.error);
    let todo_id = posted.output["message_id"].as_str().unwrap().to_string();

    let handoff = call_with_recorder(
        &runtime,
        "session_handoff_summary",
        json!({
            "session_id": coordinator.session_id,
            "include_workspace": false,
            "include_checkpoints": false,
            "include_validation": false,
            "summary_only": true
        }),
        Some(&worker.session_id),
        &auth,
        Some(&worker_window),
    )
    .await;
    assert!(handoff.success, "{:?}", handoff.error);
    assert_eq!(handoff.output["collaboration"]["open_todo_count"], 1);
    assert_eq!(
        handoff.output["collaboration"]["high_priority_open_todos"][0]["message_id"],
        todo_id
    );

    let exact = call_with_recorder(
        &runtime,
        "list_session_messages",
        json!({
            "session_id": coordinator.session_id,
            "message_id": todo_id,
            "kind": "todo",
            "status": "open",
            "limit": 1
        }),
        Some(&worker.session_id),
        &auth,
        Some(&worker_window),
    )
    .await;
    assert!(exact.success, "{:?}", exact.error);
    assert_eq!(exact.output["messages"].as_array().unwrap().len(), 1);

    let assignment_fence = runtime
        .sessions
        .get_assignment(&coordinator.session_id, &todo_id)
        .unwrap()
        .assignment_fence;
    let completed = call_with_recorder(
        &runtime,
        "complete_session_message",
        json!({
            "session_id": coordinator.session_id,
            "message_id": todo_id,
            "answer": "No findings. Revalidated the authoritative synthetic source after review.",
            "completion_key": "worker-review-v1",
            "expected_assignment_fence": assignment_fence.clone(),
            "tags": ["review", "done"],
            "priority": "normal",
            "author_session_id": "wc_sess_forged_should_be_ignored"
        }),
        Some(&worker.session_id),
        &auth,
        Some(&worker_window),
    )
    .await;
    assert!(completed.success, "{:?}", completed.error);
    assert_eq!(
        completed.output["answer"]["author_session_id"],
        worker.session_id
    );
    assert_eq!(completed.output["answer"]["reply_to"], todo_id);
    assert_eq!(
        completed.output["todo"]["resolved_by_message_id"],
        completed.output["answer_message_id"]
    );

    let replay = call_with_recorder(
        &runtime,
        "complete_session_message",
        json!({
            "session_id": coordinator.session_id,
            "message_id": todo_id,
            "answer": "No findings. Revalidated the authoritative synthetic source after review.",
            "completion_key": "worker-review-v1",
            "expected_assignment_fence": assignment_fence.clone(),
            "tags": ["review", "done"],
            "priority": "normal"
        }),
        Some(&worker.session_id),
        &auth,
        Some(&worker_window),
    )
    .await;
    assert!(replay.success, "{:?}", replay.error);
    assert_eq!(replay.output["replayed"], true);
    assert_eq!(
        replay.output["answer_message_id"],
        completed.output["answer_message_id"]
    );

    let reply = call_with_recorder(
        &runtime,
        "list_session_messages",
        json!({
            "session_id": coordinator.session_id,
            "reply_to": todo_id,
            "kind": "answer",
            "limit": 10
        }),
        Some(&coordinator.session_id),
        &auth,
        Some(&coordinator_window),
    )
    .await;
    assert!(reply.success, "{:?}", reply.error);
    assert_eq!(reply.output["messages"].as_array().unwrap().len(), 1);

    let todo_after = call_with_recorder(
        &runtime,
        "list_session_messages",
        json!({
            "session_id": coordinator.session_id,
            "message_id": todo_id,
            "kind": "todo",
            "status": "resolved",
            "limit": 1
        }),
        Some(&coordinator.session_id),
        &auth,
        Some(&coordinator_window),
    )
    .await;
    assert!(todo_after.success, "{:?}", todo_after.error);
    assert_eq!(todo_after.output["messages"].as_array().unwrap().len(), 1);

    let worker_tools = tool_names(&runtime, &worker.session_id);
    for expected in [
        "session_handoff_summary",
        "list_session_messages",
        "complete_session_message",
    ] {
        assert!(
            worker_tools.iter().any(|tool| tool == expected),
            "{expected}: {worker_tools:?}"
        );
    }
    let coordinator_tools = tool_names(&runtime, &coordinator.session_id);
    assert!(coordinator_tools
        .iter()
        .any(|tool| tool == "post_session_message"));
    assert!(!coordinator_tools
        .iter()
        .any(|tool| tool == "session_handoff_summary"));
    assert!(!coordinator_tools
        .iter()
        .any(|tool| tool == "complete_session_message"));
    let coordinator_events = runtime
        .sessions
        .summary(&coordinator.session_id, Some(100))
        .unwrap();
    let worker_events = runtime
        .sessions
        .summary(&worker.session_id, Some(100))
        .unwrap();
    let audit_text = format!(
        "{}\n{}",
        serde_json::to_string(&coordinator_events.events).unwrap(),
        serde_json::to_string(&worker_events.events).unwrap()
    );
    for private in [
        "Independent review this exact synthetic change; report findings.",
        "No findings. Revalidated the authoritative synthetic source after review.",
        "worker-review-v1",
        "wc_sess_forged_should_be_ignored",
        "collaboration-worker-window",
        assignment_fence.as_str(),
    ] {
        assert!(
            !audit_text.contains(private),
            "tool audit duplicated collaboration body or private provenance: {private}"
        );
    }

    let discussion = runtime
        .sessions
        .discussion_summary(&coordinator.session_id, Some(20))
        .unwrap();
    assert_eq!(discussion.counts.open_todos, 0);
    assert_eq!(discussion.recent_answers.len(), 1);
    assert_eq!(discussion.recent_completions.len(), 1);
    assert_eq!(
        discussion.recent_completions[0]
            .author_session_id
            .as_deref(),
        Some(worker.session_id.as_str())
    );
}

#[tokio::test]
async fn collaboration_completion_without_recording_session_has_null_author_even_with_window() {
    let client_id = "collaboration-null-author";
    let runtime = runtime_with_agent_project(client_id);
    register_agent(&runtime, client_id, None, RunnerCapabilities::default()).await;
    let project = agent_test_project_id(client_id);
    let auth = auth_context(None, true);
    let coordinator = start_authorized_project_session(&runtime, &project, None, &auth);
    let todo = runtime
        .sessions
        .post_message(PostSessionMessageInput {
            session_id: coordinator.session_id.clone(),
            kind: SessionMessageKind::Todo,
            message: "window must not infer author Session".to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::Normal,
        })
        .unwrap();

    let assignment_fence = runtime
        .sessions
        .get_assignment(&coordinator.session_id, &todo.message_id)
        .unwrap()
        .assignment_fence;
    let window = ClientWindow::for_test("no-recorder-author-window");
    let result = call_with_recorder(
        &runtime,
        "complete_session_message",
        json!({
            "session_id": coordinator.session_id,
            "message_id": todo.message_id,
            "answer": "done",
            "completion_key": "no-window",
            "expected_assignment_fence": assignment_fence
        }),
        None,
        &auth,
        Some(&window),
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["answer"]["author_session_id"], Value::Null);
}

#[tokio::test]
async fn collaboration_explicit_recording_session_is_completion_author() {
    let client_id = "collaboration-recorder-author";
    let runtime = runtime_with_agent_project(client_id);
    register_agent(&runtime, client_id, None, RunnerCapabilities::default()).await;
    let project = agent_test_project_id(client_id);
    let auth = auth_context(None, true);
    let coordinator =
        start_authorized_project_session(&runtime, &project, Some("coordinator"), &auth);
    let worker_recording =
        start_authorized_project_session(&runtime, &project, Some("worker W1"), &auth);
    let todo = runtime
        .sessions
        .post_message(PostSessionMessageInput {
            session_id: coordinator.session_id.clone(),
            kind: SessionMessageKind::Todo,
            message: "prove recorder provenance precedence".to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::High,
        })
        .unwrap();
    let assignment_fence = runtime
        .sessions
        .get_assignment(&coordinator.session_id, &todo.message_id)
        .unwrap()
        .assignment_fence;
    let window = ClientWindow::for_test("recorder-vs-current-window");
    let completed = call_with_recorder(
        &runtime,
        "complete_session_message",
        json!({
            "session_id": coordinator.session_id,
            "message_id": todo.message_id,
            "answer": "completed by the recording Session",
            "completion_key": "recorder-wins-v1",
            "expected_assignment_fence": assignment_fence
        }),
        Some(&worker_recording.session_id),
        &auth,
        Some(&window),
    )
    .await;
    assert!(completed.success, "{:?}", completed.error);
    assert_eq!(
        completed.output["answer"]["author_session_id"],
        worker_recording.session_id
    );
    let recording_tools = tool_names(&runtime, &worker_recording.session_id);
    assert!(recording_tools
        .iter()
        .any(|tool| tool == "complete_session_message"));
    let coordinator_tools = tool_names(&runtime, &coordinator.session_id);
    assert!(!coordinator_tools
        .iter()
        .any(|tool| tool == "complete_session_message"));

    let discussion = runtime
        .sessions
        .discussion_summary(&coordinator.session_id, Some(10))
        .unwrap();
    assert_eq!(discussion.recent_completions.len(), 1);
    assert_eq!(
        discussion.recent_completions[0]
            .author_session_id
            .as_deref(),
        Some(worker_recording.session_id.as_str())
    );
}

#[tokio::test]
async fn collaboration_cross_project_recorder_fails_closed_before_completion() {
    let runtime = runtime_with_resolver_projects().await;
    let auth = auth_context(None, true);
    let coordinator =
        start_authorized_project_session(&runtime, "agent:workstation:my-repo", Some("C"), &auth);
    let worker = start_authorized_project_session(
        &runtime,
        "agent:workstation:other-repo",
        Some("W"),
        &auth,
    );
    let todo = runtime
        .sessions
        .post_message(PostSessionMessageInput {
            session_id: coordinator.session_id.clone(),
            kind: SessionMessageKind::Todo,
            message: "must not cross projects".to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::High,
        })
        .unwrap();
    let assignment_fence = runtime
        .sessions
        .get_assignment(&coordinator.session_id, &todo.message_id)
        .unwrap()
        .assignment_fence;
    let window = ClientWindow::for_test("cross-project-worker");
    let result = call_with_recorder(
        &runtime,
        "complete_session_message",
        json!({
            "session_id": coordinator.session_id,
            "message_id": todo.message_id,
            "answer": "should not commit",
            "completion_key": "cross-project",
            "expected_assignment_fence": assignment_fence
        }),
        Some(&worker.session_id),
        &auth,
        Some(&window),
    )
    .await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "session_project_mismatch");
    let still_open = runtime
        .sessions
        .list_messages(
            &coordinator.session_id,
            sessions::ListSessionMessagesFilter {
                message_id: Some(todo.message_id),
                status: Some(sessions::SessionMessageStatus::Open),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(still_open.len(), 1);
    assert_eq!(
        runtime
            .sessions
            .list_messages(
                &coordinator.session_id,
                sessions::ListSessionMessagesFilter {
                    kind: Some(SessionMessageKind::Answer),
                    ..Default::default()
                },
            )
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn foreign_recording_session_is_denied_before_ordinary_tool_recording() {
    let runtime = test_runtime();
    let alice = shared_key_auth_context("ordinary-recorder-alice");
    let bob = shared_key_auth_context("ordinary-recorder-bob");
    let started = call_with_recorder(
        &runtime,
        "start_session",
        json!({"title": "Alice recorder"}),
        None,
        &alice,
        None,
    )
    .await;
    assert!(started.success, "{:?}", started.error);
    let session_id = started.output["session_id"].as_str().unwrap().to_string();
    let before = runtime.sessions.summary(&session_id, Some(100)).unwrap();

    let denied = call_with_recorder(
        &runtime,
        "list_projects",
        json!({}),
        Some(&session_id),
        &bob,
        None,
    )
    .await;
    assert!(!denied.success);
    assert_eq!(denied.output["error_kind"], "session_authority_denied");
    let after_denial = runtime.sessions.summary(&session_id, Some(100)).unwrap();
    assert_eq!(after_denial.events.len(), before.events.len());
    assert!(!tool_names(&runtime, &session_id)
        .iter()
        .any(|tool| tool == "list_projects"));

    let allowed = call_with_recorder(
        &runtime,
        "list_projects",
        json!({}),
        Some(&session_id),
        &alice,
        None,
    )
    .await;
    assert!(allowed.success, "{:?}", allowed.error);
    let after_allowed = runtime.sessions.summary(&session_id, Some(100)).unwrap();
    assert_eq!(after_allowed.events.len(), before.events.len() + 2);
    assert!(tool_names(&runtime, &session_id)
        .iter()
        .any(|tool| tool == "list_projects"));
}

#[tokio::test]
async fn collaboration_mixed_project_scope_fails_closed_in_both_directions() {
    let runtime = runtime_with_resolver_projects().await;
    let auth = auth_context(None, true);

    let scoped_coordinator = start_authorized_project_session(
        &runtime,
        "agent:workstation:my-repo",
        Some("scoped coordinator"),
        &auth,
    );
    let unscoped_worker = call_with_recorder(
        &runtime,
        "start_session",
        json!({"title": "unscoped worker"}),
        None,
        &auth,
        None,
    )
    .await;
    assert!(unscoped_worker.success, "{:?}", unscoped_worker.error);
    let unscoped_worker_id = unscoped_worker.output["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let scoped_todo = runtime
        .sessions
        .post_message(PostSessionMessageInput {
            session_id: scoped_coordinator.session_id.clone(),
            kind: SessionMessageKind::Todo,
            message: "unscoped worker must not bridge into scoped coordinator".to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::High,
        })
        .unwrap();
    let scoped_assignment_fence = runtime
        .sessions
        .get_assignment(&scoped_coordinator.session_id, &scoped_todo.message_id)
        .unwrap()
        .assignment_fence;
    for (tool_name, arguments) in [
        (
            "list_session_messages",
            json!({"session_id": scoped_coordinator.session_id, "message_id": scoped_todo.message_id}),
        ),
        (
            "complete_session_message",
            json!({
                "session_id": scoped_coordinator.session_id,
                "message_id": scoped_todo.message_id,
                "answer": "must not complete",
                "completion_key": "mixed-unscoped-to-scoped",
                "expected_assignment_fence": scoped_assignment_fence.clone()
            }),
        ),
    ] {
        let denied = call_with_recorder(
            &runtime,
            tool_name,
            arguments,
            Some(&unscoped_worker_id),
            &auth,
            None,
        )
        .await;
        assert!(!denied.success, "{tool_name} unexpectedly succeeded");
        assert_eq!(denied.output["error_kind"], "session_project_mismatch");
    }
    assert_eq!(
        runtime
            .sessions
            .list_messages(
                &scoped_coordinator.session_id,
                sessions::ListSessionMessagesFilter {
                    kind: Some(SessionMessageKind::Answer),
                    ..Default::default()
                },
            )
            .unwrap()
            .len(),
        0
    );

    let unscoped_coordinator = call_with_recorder(
        &runtime,
        "start_session",
        json!({"title": "unscoped coordinator"}),
        None,
        &auth,
        None,
    )
    .await;
    assert!(
        unscoped_coordinator.success,
        "{:?}",
        unscoped_coordinator.error
    );
    let unscoped_coordinator_id = unscoped_coordinator.output["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let scoped_worker = start_authorized_project_session(
        &runtime,
        "agent:workstation:my-repo",
        Some("scoped worker"),
        &auth,
    );
    let unscoped_todo = runtime
        .sessions
        .post_message(PostSessionMessageInput {
            session_id: unscoped_coordinator_id.clone(),
            kind: SessionMessageKind::Todo,
            message: "scoped worker must not bridge into unscoped coordinator".to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::High,
        })
        .unwrap();
    let unscoped_assignment_fence = runtime
        .sessions
        .get_assignment(&unscoped_coordinator_id, &unscoped_todo.message_id)
        .unwrap()
        .assignment_fence;
    for (tool_name, arguments) in [
        (
            "list_session_messages",
            json!({"session_id": unscoped_coordinator_id, "message_id": unscoped_todo.message_id}),
        ),
        (
            "complete_session_message",
            json!({
                "session_id": unscoped_coordinator_id,
                "message_id": unscoped_todo.message_id,
                "answer": "must not complete",
                "completion_key": "mixed-scoped-to-unscoped",
                "expected_assignment_fence": unscoped_assignment_fence.clone()
            }),
        ),
    ] {
        let denied = call_with_recorder(
            &runtime,
            tool_name,
            arguments,
            Some(&scoped_worker.session_id),
            &auth,
            None,
        )
        .await;
        assert!(!denied.success, "{tool_name} unexpectedly succeeded");
        assert_eq!(denied.output["error_kind"], "session_project_mismatch");
    }
    assert_eq!(
        runtime
            .sessions
            .list_messages(
                &unscoped_coordinator_id,
                sessions::ListSessionMessagesFilter {
                    kind: Some(SessionMessageKind::Answer),
                    ..Default::default()
                },
            )
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn project_scoped_session_authority_rejects_recycled_project_identity() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = dir.path().join("sessions.json");
    let runner_registry =
        Arc::new(crate::runner_http::RunnerRegistry::with_shared_key_limits_for_test(4, 8, 1));
    let runtime = ToolRuntime::new_for_tests_with_runner_registry(runner_registry.clone())
        .with_session_ledger(&ledger);
    let alice = shared_key_auth_context("recycled-authority-a");
    let alice_oauth = oauth_bridge_auth_context(
        "recycled-authority-a",
        &[
            crate::auth::SCOPE_RUNTIME_READ,
            crate::auth::SCOPE_SESSION_COLLABORATE,
            crate::auth::SCOPE_PROJECT_READ,
            crate::auth::SCOPE_PROJECT_WRITE,
            crate::auth::SCOPE_JOB_RUN,
            crate::auth::SCOPE_AGENT_REGISTER,
        ],
    );
    let bob = shared_key_auth_context("recycled-authority-b");
    assert_eq!(
        super::super::session_context::workflow_session_authority_fingerprint(Some(&alice))
            .unwrap(),
        super::super::session_context::workflow_session_authority_fingerprint(Some(&alice_oauth))
            .unwrap(),
        "direct shared-key and OAuth bridge must be one canonical authority group"
    );

    let project_summary = named_registered_project(
        "recycled-client",
        "recycled-project",
        "Recycled Project",
        "/tmp/recycled-project",
        1,
    );
    register_agent_projects_for_auth(
        &runtime,
        "recycled-client",
        &alice,
        RunnerCapabilities::default(),
        vec![project_summary.clone()],
    )
    .await;
    let project = "agent:recycled-client:recycled-project".to_string();
    let started = call_with_recorder(
        &runtime,
        "start_session",
        json!({"project": project, "title": "recycling fence"}),
        None,
        &alice,
        None,
    )
    .await;
    assert!(started.success, "{:?}", started.error);
    let session_id = started.output["session_id"].as_str().unwrap().to_string();
    let posted = call_with_recorder(
        &runtime,
        "post_session_message",
        json!({
            "session_id": session_id,
            "kind": "todo",
            "message": "authority recycling must not resolve this todo"
        }),
        None,
        &alice,
        None,
    )
    .await;
    assert!(posted.success, "{:?}", posted.error);
    let todo_id = posted.output["message_id"].as_str().unwrap().to_string();
    let assignment_fence = runtime
        .sessions
        .get_assignment(&session_id, &todo_id)
        .unwrap()
        .assignment_fence;
    runtime.sessions.flush_persistence();
    let persisted = std::fs::read_to_string(&ledger).unwrap();
    assert!(persisted.contains("owner_authority_fingerprint"));
    assert!(!persisted.contains("recycled-authority-a"));

    let oauth_read = call_with_recorder(
        &runtime,
        "session_summary",
        json!({"session_id": session_id}),
        None,
        &alice_oauth,
        None,
    )
    .await;
    assert!(oauth_read.success, "{:?}", oauth_read.error);

    let expired_at = chrono::Utc::now().timestamp() - 100;
    runner_registry
        .set_last_seen_for_test("recycled-client", expired_at)
        .await;
    let _ = runner_registry
        .list_runners_for_auth(Some(&crate::test_support::runner_access(&alice)))
        .await;
    assert!(runner_registry
        .get_runner_view("recycled-client")
        .await
        .is_none());

    register_agent_projects_for_auth(
        &runtime,
        "recycled-client",
        &bob,
        RunnerCapabilities::default(),
        vec![project_summary.clone()],
    )
    .await;
    let before = runtime.sessions.summary(&session_id, Some(100)).unwrap();
    let denied_calls = [
        ("session_summary", json!({"session_id": session_id})),
        (
            "list_session_messages",
            json!({"session_id": session_id, "message_id": todo_id}),
        ),
        (
            "post_session_message",
            json!({"session_id": session_id, "kind": "note", "message": "Bob must not write"}),
        ),
        (
            "complete_session_message",
            json!({
                "session_id": session_id,
                "message_id": todo_id,
                "answer": "Bob must not complete",
                "completion_key": "recycled-bob",
                "expected_assignment_fence": assignment_fence.clone()
            }),
        ),
        (
            "session_discussion_summary",
            json!({"session_id": session_id}),
        ),
        (
            "session_handoff_summary",
            json!({
                "session_id": session_id,
                "include_workspace": false,
                "include_checkpoints": false,
                "include_validation": false,
                "summary_only": true
            }),
        ),
        ("close_session", json!({"session_id": session_id})),
    ];
    for (tool_name, arguments) in denied_calls {
        let denied = call_with_recorder(&runtime, tool_name, arguments, None, &bob, None).await;
        assert!(!denied.success, "{tool_name} unexpectedly succeeded");
        assert_eq!(
            denied.output["error_kind"], "session_authority_denied",
            "{tool_name}: {:?}",
            denied.output
        );
        let denied_state = runtime.sessions.summary(&session_id, Some(100)).unwrap();
        assert_eq!(
            denied_state.events.len(),
            before.events.len(),
            "{tool_name} denial must not append Session evidence: {:?}",
            tool_names(&runtime, &session_id)
        );
        assert_eq!(
            denied_state.messages.total, before.messages.total,
            "{tool_name} denial must not mutate Session messages"
        );
        assert_eq!(
            denied_state.lifecycle, before.lifecycle,
            "{tool_name} denial must not mutate Session lifecycle"
        );
    }
    let after = runtime.sessions.summary(&session_id, Some(100)).unwrap();
    assert_eq!(after.lifecycle, before.lifecycle);
    assert_eq!(after.messages.total, before.messages.total);
    assert_eq!(after.events.len(), before.events.len());
    assert_eq!(
        runtime
            .sessions
            .list_messages(
                &session_id,
                sessions::ListSessionMessagesFilter {
                    kind: Some(SessionMessageKind::Answer),
                    ..Default::default()
                },
            )
            .unwrap()
            .len(),
        0
    );

    runner_registry
        .set_last_seen_for_test("recycled-client", expired_at)
        .await;
    let _ = runner_registry
        .list_runners_for_auth(Some(&crate::test_support::runner_access(&bob)))
        .await;
    assert!(runner_registry
        .get_runner_view("recycled-client")
        .await
        .is_none());
    register_agent_projects_for_auth(
        &runtime,
        "recycled-client",
        &alice,
        RunnerCapabilities::default(),
        vec![project_summary],
    )
    .await;
    let restored_owner = call_with_recorder(
        &runtime,
        "session_summary",
        json!({"session_id": session_id}),
        None,
        &alice,
        None,
    )
    .await;
    assert!(restored_owner.success, "{:?}", restored_owner.error);
}

#[tokio::test]
async fn projectless_session_owner_authority_blocks_known_ids_from_foreign_principal() {
    let runtime = test_runtime();
    let alice = shared_key_auth_context("projectless-owner-alice");
    let bob = shared_key_auth_context("projectless-owner-bob");

    let started = call_with_recorder(
        &runtime,
        "start_session",
        json!({"title": "Alice project-less Session"}),
        None,
        &alice,
        None,
    )
    .await;
    assert!(started.success, "{:?}", started.error);
    let session_id = started.output["session_id"].as_str().unwrap().to_string();
    assert_eq!(started.output["project"], Value::Null);

    let posted = call_with_recorder(
        &runtime,
        "post_session_message",
        json!({
            "session_id": session_id,
            "kind": "todo",
            "message": "Alice-owned project-less todo",
            "priority": "high"
        }),
        None,
        &alice,
        None,
    )
    .await;
    assert!(posted.success, "{:?}", posted.error);
    let todo_id = posted.output["message_id"].as_str().unwrap().to_string();
    let assignment_fence = runtime
        .sessions
        .get_assignment(&session_id, &todo_id)
        .unwrap()
        .assignment_fence;

    let denied_calls = [
        ("session_summary", json!({"session_id": session_id})),
        (
            "post_session_message",
            json!({"session_id": session_id, "kind": "note", "message": "Bob note"}),
        ),
        (
            "list_session_messages",
            json!({"session_id": session_id, "message_id": todo_id}),
        ),
        (
            "resolve_session_message",
            json!({"session_id": session_id, "message_id": todo_id}),
        ),
        (
            "complete_session_message",
            json!({
                "session_id": session_id,
                "message_id": todo_id,
                "answer": "Bob forged completion",
                "completion_key": "bob-forged",
                "expected_assignment_fence": assignment_fence.clone()
            }),
        ),
        (
            "session_discussion_summary",
            json!({"session_id": session_id}),
        ),
        (
            "session_handoff_summary",
            json!({
                "session_id": session_id,
                "include_workspace": false,
                "include_checkpoints": false,
                "include_validation": false,
                "summary_only": true
            }),
        ),
        ("close_session", json!({"session_id": session_id})),
    ];
    for (tool_name, arguments) in denied_calls {
        let denied = call_with_recorder(&runtime, tool_name, arguments, None, &bob, None).await;
        assert!(!denied.success, "{tool_name} unexpectedly succeeded");
        assert_eq!(
            denied.output["error_kind"], "session_authority_denied",
            "{tool_name}: {:?}",
            denied.output
        );
    }

    let alice_summary = call_with_recorder(
        &runtime,
        "session_summary",
        json!({"session_id": session_id}),
        None,
        &alice,
        None,
    )
    .await;
    assert!(alice_summary.success, "{:?}", alice_summary.error);
    let alice_complete = call_with_recorder(
        &runtime,
        "complete_session_message",
        json!({
            "session_id": session_id,
            "message_id": todo_id,
            "answer": "Alice completion",
            "completion_key": "alice-complete",
            "expected_assignment_fence": assignment_fence
        }),
        None,
        &alice,
        None,
    )
    .await;
    assert!(alice_complete.success, "{:?}", alice_complete.error);
    let alice_close = call_with_recorder(
        &runtime,
        "close_session",
        json!({"session_id": session_id}),
        None,
        &alice,
        None,
    )
    .await;
    assert!(alice_close.success, "{:?}", alice_close.error);
}

#[tokio::test]
async fn projectless_owner_fingerprint_survives_restart_without_raw_principal_material() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = dir.path().join("sessions.json");
    let alice_identity = "projectless-persisted-alice-raw-principal";
    let alice = shared_key_auth_context(alice_identity);
    let bob = shared_key_auth_context("projectless-persisted-bob");
    let runtime = test_runtime().with_session_ledger(&ledger);

    let started = call_with_recorder(
        &runtime,
        "start_session",
        json!({"title": "durable project-less owner"}),
        None,
        &alice,
        None,
    )
    .await;
    assert!(started.success, "{:?}", started.error);
    let session_id = started.output["session_id"].as_str().unwrap().to_string();
    runtime.sessions.flush_persistence();
    let persisted = std::fs::read_to_string(&ledger).unwrap();
    assert!(persisted.contains("owner_authority_fingerprint"));
    assert!(!persisted.contains(alice_identity));
    drop(runtime);

    let restored = test_runtime().with_session_ledger(&ledger);
    let alice_read = call_with_recorder(
        &restored,
        "session_summary",
        json!({"session_id": session_id}),
        None,
        &alice,
        None,
    )
    .await;
    assert!(alice_read.success, "{:?}", alice_read.error);
    let bob_read = call_with_recorder(
        &restored,
        "session_summary",
        json!({"session_id": session_id}),
        None,
        &bob,
        None,
    )
    .await;
    assert!(!bob_read.success);
    assert_eq!(bob_read.output["error_kind"], "session_authority_denied");
}

#[tokio::test]
async fn legacy_projectless_owner_hash_is_not_accepted_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = dir.path().join("sessions.json");
    let alice = shared_key_auth_context("legacy-owner-alice");
    let runtime = test_runtime().with_session_ledger(&ledger);
    let started = call_with_recorder(
        &runtime,
        "start_session",
        json!({"title": "canonical project-less Session"}),
        None,
        &alice,
        None,
    )
    .await;
    assert!(started.success, "{:?}", started.error);
    let session_id = started.output["session_id"].as_str().unwrap().to_string();
    runtime.sessions.flush_persistence();
    drop(runtime);

    let authority_id = alice.shared_key_hash.as_deref().unwrap();
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex.workflow-session-owner.v1\0");
    hasher.update(alice.principal_kind().as_bytes());
    hasher.update(b"\0");
    hasher.update(authority_id.as_bytes());
    let legacy_fingerprint = format!("{:x}", hasher.finalize());
    let canonical_fingerprint =
        super::super::session_context::workflow_session_authority_fingerprint(Some(&alice))
            .unwrap();
    assert_ne!(legacy_fingerprint, canonical_fingerprint);

    let mut persisted: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger).unwrap()).unwrap();
    let row = persisted["sessions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|row| row["session_id"] == session_id)
        .unwrap();
    row["owner_authority_fingerprint"] = Value::String(legacy_fingerprint);
    std::fs::write(&ledger, serde_json::to_vec_pretty(&persisted).unwrap()).unwrap();

    let restored = test_runtime().with_session_ledger(&ledger);
    assert!(restored.sessions.summary(&session_id, None).is_some());
    let denied = call_with_recorder(
        &restored,
        "session_summary",
        json!({"session_id": session_id}),
        None,
        &alice,
        None,
    )
    .await;
    assert!(!denied.success);
    assert_eq!(denied.output["error_kind"], "session_authority_denied");
}

#[tokio::test]
async fn collaboration_foreign_owner_cannot_read_or_complete_known_session_and_todo_ids() {
    let runtime = test_runtime();
    let alice = shared_key_auth_context("collaboration-owner-a");
    let bob = shared_key_auth_context("collaboration-owner-b");
    register_agent_projects_for_auth(
        &runtime,
        "alice-host",
        &alice,
        RunnerCapabilities::default(),
        vec![named_registered_project(
            "alice-host",
            "private",
            "Private",
            "/tmp/alice-private",
            1,
        )],
    )
    .await;
    let project = "agent:alice-host:private".to_string();
    let coordinator = start_authorized_project_session(&runtime, &project, None, &alice);
    let todo = runtime
        .sessions
        .post_message(PostSessionMessageInput {
            session_id: coordinator.session_id.clone(),
            kind: SessionMessageKind::Todo,
            message: "known ids are not capabilities".to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::Normal,
        })
        .unwrap();
    let assignment_fence = runtime
        .sessions
        .get_assignment(&coordinator.session_id, &todo.message_id)
        .unwrap()
        .assignment_fence;

    let read = call_with_recorder(
        &runtime,
        "list_session_messages",
        json!({
            "session_id": coordinator.session_id,
            "message_id": todo.message_id
        }),
        None,
        &bob,
        None,
    )
    .await;
    assert!(!read.success);

    let complete = call_with_recorder(
        &runtime,
        "complete_session_message",
        json!({
            "session_id": coordinator.session_id,
            "message_id": todo.message_id,
            "answer": "forged completion",
            "completion_key": "foreign",
            "expected_assignment_fence": assignment_fence
        }),
        None,
        &bob,
        None,
    )
    .await;
    assert!(!complete.success);
    assert_eq!(
        runtime
            .sessions
            .list_messages(
                &coordinator.session_id,
                sessions::ListSessionMessagesFilter {
                    kind: Some(SessionMessageKind::Answer),
                    ..Default::default()
                },
            )
            .unwrap()
            .len(),
        0
    );
}
