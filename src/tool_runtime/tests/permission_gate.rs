//! Phase 2 permission pre-exec gate: single evaluation, mode matrix, mutation side effects.

use super::super::kernel::{ToolCallContext, ToolCallRequest, ToolTransport};
use super::super::permissions::{AuthorityMode, EffectiveAuthorityConfig, PermissionEvaluator};
use super::super::*;
use super::support::*;
use crate::runner_protocol::RunnerCapabilities;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn write_tool_call(
    project: String,
    path: &str,
    content: &str,
    session_id: Option<String>,
) -> ToolCall {
    ToolCall::WriteProjectFile {
        project,
        path: path.to_string(),
        content: content.to_string(),
        session_id,
        overwrite: None,
        expected_sha256: None,
    }
}

fn runtime_with_mode(client_id: &str, mode: AuthorityMode) -> ToolRuntime {
    runtime_with_agent_project(client_id)
        .with_permission_evaluator(PermissionEvaluator::with_mode(mode))
}

fn runtime_with_config(client_id: &str, config: EffectiveAuthorityConfig) -> ToolRuntime {
    runtime_with_agent_project(client_id)
        .with_permission_evaluator(PermissionEvaluator::with_config(config))
}

async fn register_write_agent(runtime: &ToolRuntime, client_id: &str) {
    register_agent(
        runtime,
        client_id,
        None,
        RunnerCapabilities {
            file_write: true,
            shell: true,
            git: true,
            ..Default::default()
        },
    )
    .await;
}

/// Complete a successful agent write so the mutation path can finish.
async fn complete_write_ok(runtime: &ToolRuntime, client_id: &str, path: &str) {
    let req = wait_for_patch_agent_request(runtime, client_id).await;
    assert_eq!(req.kind, "file_write_project_file");
    let payload: serde_json::Value =
        serde_json::from_str(req.content.as_deref().expect("file-op payload")).unwrap();
    assert_eq!(payload["path"], path);
    complete_patch_agent_request(
        runtime,
        client_id,
        &req.request_id,
        0,
        &format!(r#"{{"path":"{path}","bytes_written":4,"sha256":"abc","changed":true,"state_changed":true,"execution_state":"completed"}}"#),
        "",
    )
    .await;
}

#[tokio::test]
async fn trusted_agent_evaluates_once_executes_and_attaches_same_decision() {
    let counter = Arc::new(AtomicUsize::new(0));
    let client_id = "perm-auto-once";
    let runtime = runtime_with_agent_project(client_id).with_permission_evaluator(
        PermissionEvaluator::with_mode(AuthorityMode::TrustedAgent)
            .with_eval_counter(counter.clone()),
    );
    register_write_agent(&runtime, client_id).await;
    let project = agent_test_project_id(client_id);
    let session = runtime.sessions.start_session(Some(project.clone()), None);
    let bootstrap = auth_context(None, true);

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    write_tool_call(project, "src/once.txt", "hi\n", Some(session_id)),
                    Some(&bootstrap),
                )
                .await
        }
    });
    complete_write_ok(&runtime, client_id, "src/once.txt").await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(counter.load(Ordering::SeqCst), 1, "evaluator must run once");
    assert_eq!(result.output["permission"]["status"], "auto_approved");
    assert_eq!(result.output["permission"]["policy"], "trusted_agent");
    assert_eq!(
        result.output["permission"]["reason"],
        "trusted_agent_authority"
    );
    let request_id = result.output["permission"]["request_id"]
        .as_str()
        .expect("request_id");
    assert!(request_id.starts_with("wc_perm_"), "{request_id}");

    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    let event = finished_event(&summary, "write_project_file");
    let ledger_perm = event.permission.as_ref().expect("ledger permission");
    assert_eq!(ledger_perm.request_id, request_id);
    assert_eq!(ledger_perm.status, "auto_approved");
}

#[tokio::test]
async fn restricted_blocks_mutation_before_agent_enqueue() {
    let client_id = "perm-require";
    let runtime = runtime_with_mode(client_id, AuthorityMode::Restricted);
    register_write_agent(&runtime, client_id).await;
    let project = agent_test_project_id(client_id);
    let session = runtime.sessions.start_session(Some(project.clone()), None);
    let bootstrap = auth_context(None, true);

    let result = runtime
        .dispatch_with_auth(
            write_tool_call(
                project,
                "src/blocked.txt",
                "must-not-write\n",
                Some(session.session_id.clone()),
            ),
            Some(&bootstrap),
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "permission_denied");
    assert_eq!(result.output["failure_kind"], "permission_denied");
    assert_eq!(result.output["permission"]["status"], "denied");
    assert_eq!(
        result.output["permission"]["reason"],
        "restricted_requires_human_authorization"
    );
    assert_ne!(result.output["permission"]["status"], "auto_approved");
    let err = result.error.as_deref().unwrap();
    assert!(
        err.contains("restricted authority mode requires human authorization"),
        "{err}"
    );

    assert!(
        probe_patch_agent_request(&runtime, client_id)
            .await
            .is_none(),
        "restricted authority must not enqueue mutation"
    );

    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    let event = finished_event(&summary, "write_project_file");
    assert_eq!(event.status.as_deref(), Some("failed"));
    let perm = event.permission.as_ref().expect("permission on ledger");
    assert_eq!(perm.status, "denied");
    assert_eq!(perm.reason, "restricted_requires_human_authorization");
}

#[tokio::test]
async fn invalid_mode_blocks_mutation_and_does_not_auto_approve() {
    let client_id = "perm-invalid";
    let runtime = runtime_with_config(
        client_id,
        EffectiveAuthorityConfig::from_raw(Some("totally_bogus_mode")),
    );
    register_write_agent(&runtime, client_id).await;
    let project = agent_test_project_id(client_id);
    let bootstrap = auth_context(None, true);

    let result = runtime
        .dispatch_with_auth(
            write_tool_call(project, "src/invalid-mode.txt", "nope\n", None),
            Some(&bootstrap),
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "permission_denied");
    assert_eq!(result.output["permission"]["status"], "denied");
    assert_ne!(result.output["permission"]["status"], "auto_approved");
    assert!(
        result.output["permission"]["reason"]
            .as_str()
            .unwrap()
            .contains("invalid_authority_mode"),
        "{:?}",
        result.output["permission"]["reason"]
    );
    let err = result.error.as_deref().unwrap();
    assert!(
        err.contains("WEBCODEX_AUTHORITY_MODE") || err.contains("invalid"),
        "{err}"
    );
    assert!(
        probe_patch_agent_request(&runtime, client_id)
            .await
            .is_none(),
        "invalid mode must not enqueue mutation"
    );
}

#[tokio::test]
async fn hard_policy_deny_still_suppresses_permission_attach() {
    let client_id = "perm-hard-deny";
    let runtime = runtime_with_mode(client_id, AuthorityMode::TrustedAgent);
    register_write_agent(&runtime, client_id).await;
    let project = agent_test_project_id(client_id);
    let session = runtime.sessions.start_session(Some(project.clone()), None);
    let bootstrap = auth_context(None, true);

    // Sensitive / policy-rejected path is hard safety inside the tool; must
    // not attach auto_approved over a hard deny.
    let result = runtime
        .dispatch_with_auth(
            ToolCall::ArtifactUploadBegin {
                project,
                path: "artifacts/smoke/raw.bin".to_string(),
                session_id: Some(session.session_id.clone()),
                expected_bytes: Some(1),
                expected_sha256: None,
                mime_type: Some("application/octet-stream".to_string()),
                overwrite: Some(false),
            },
            Some(&bootstrap),
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.output["failure_kind"], "policy_rejected");
    assert_eq!(result.output["error_kind"], "policy_rejected");
    assert!(
        result.output.get("permission").is_none(),
        "hard deny must not carry permission auto-approve: {:?}",
        result.output.get("permission")
    );
    assert!(
        probe_patch_agent_request(&runtime, client_id)
            .await
            .is_none(),
        "policy rejection must happen before enqueue"
    );
    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    let event = finished_event(&summary, "artifact_upload_begin");
    assert!(event.permission.is_none());
}

#[tokio::test]
async fn observe_tool_skips_permission_evaluator() {
    let counter = Arc::new(AtomicUsize::new(0));
    let client_id = "perm-observe";
    let runtime = runtime_with_agent_project(client_id).with_permission_evaluator(
        PermissionEvaluator::with_mode(AuthorityMode::Restricted)
            .with_eval_counter(counter.clone()),
    );
    register_agent(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            file_read: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id(client_id);
    let bootstrap = auth_context(None, true);

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFiles {
                        project: project,
                        items: vec![crate::tool_runtime::ReadFilesItem {
                            path: "README.md".to_string(),
                            start_line: None,
                            limit: None,
                        }],
                        session_id: None,
                        with_line_numbers: None,
                        max_result_bytes: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, client_id).await;
    complete_patch_agent_request(
        &runtime,
        client_id,
        &req.request_id,
        0,
        &canonical_agent_file_read_output("hello\n", 1),
        "",
    )
    .await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "ApprovalPolicy::None must skip PermissionEvaluator"
    );
    assert!(
        result.output.get("permission").is_none(),
        "observe tools must not invent permission records: {:?}",
        result.output.get("permission")
    );
}

#[tokio::test]
async fn session_mutation_with_no_approval_skips_permission_evaluator() {
    let counter = Arc::new(AtomicUsize::new(0));
    let runtime = test_runtime().with_permission_evaluator(
        PermissionEvaluator::with_mode(AuthorityMode::Restricted)
            .with_eval_counter(counter.clone()),
    );
    let session = runtime
        .sessions
        .start_session(None, Some("permission semantic test".to_string()));

    let result = runtime
        .dispatch(ToolCall::CloseSession {
            session_id: session.session_id.clone(),
        })
        .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["lifecycle"], "closed");
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    assert!(
        result.output.get("permission").is_none(),
        "Mutate + ApprovalPolicy::None must not create an interactive permission record"
    );
}

#[tokio::test]
async fn coding_agent_cancel_inherits_start_approval_without_second_evaluator_decision() {
    let counter = Arc::new(AtomicUsize::new(0));
    let runtime = test_runtime().with_permission_evaluator(
        PermissionEvaluator::with_mode(AuthorityMode::Restricted)
            .with_eval_counter(counter.clone()),
    );
    let auth = auth_context(None, true);

    let result = runtime
        .dispatch_with_auth(
            ToolCall::CodingAgentCancel {
                run_id: "wc_car_permission_semantics_missing".to_string(),
            },
            Some(&auth),
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "unknown_coding_agent_run");
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "InheritFromStart must not trigger a second PermissionEvaluator decision"
    );
    assert!(result.output.get("permission").is_none());
}

#[tokio::test]
async fn kernel_path_preserves_permission_denial_for_model_recovery() {
    let client_id = "perm-kernel-denied";
    let runtime = runtime_with_mode(client_id, AuthorityMode::Restricted);
    register_write_agent(&runtime, client_id).await;
    let project = agent_test_project_id(client_id);
    let recording = runtime.sessions.start_session(Some(project.clone()), None);
    let bootstrap = auth_context(None, true);
    let recording_id = recording.session_id.clone();

    let outcome = runtime
        .call_tool_with_context(
            ToolCallRequest {
                tool_name: "write_project_file".to_string(),
                arguments: json!({
                    "project": project,
                    "path": "src/kernel-denied.txt",
                    "content": "nope\n",
                    "session_id": recording_id,
                }),
            },
            ToolCallContext {
                transport: ToolTransport::Api,
                session_id: Some(&recording_id),
                auth: Some(&bootstrap),
                window: None,
                record_oauth_scope_denials: true,
                host_file_import_trust: crate::tool_runtime::kernel::HostFileImportTrust::Untrusted,
            },
        )
        .await;

    assert!(!outcome.success);
    let result = outcome.result.expect("permission denial tool result");
    assert_eq!(result.output["error_kind"], "permission_denied");
    assert_eq!(result.output["permission"]["status"], "denied");
    assert_eq!(
        result.output["permission"]["reason"],
        "restricted_requires_human_authorization"
    );
    assert!(probe_patch_agent_request(&runtime, client_id)
        .await
        .is_none());
}

#[tokio::test]
async fn kernel_path_does_not_double_evaluate_or_duplicate_request_id() {
    let counter = Arc::new(AtomicUsize::new(0));
    let client_id = "perm-kernel-once";
    let runtime = runtime_with_agent_project(client_id).with_permission_evaluator(
        PermissionEvaluator::with_mode(AuthorityMode::TrustedAgent)
            .with_eval_counter(counter.clone()),
    );
    register_write_agent(&runtime, client_id).await;
    let project = agent_test_project_id(client_id);
    let recording = runtime.sessions.start_session(Some(project.clone()), None);
    let bootstrap = auth_context(None, true);

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let recording_id = recording.session_id.clone();
        let bootstrap = bootstrap.clone();
        async move {
            runtime
                .call_tool_with_context(
                    ToolCallRequest {
                        tool_name: "write_project_file".to_string(),
                        arguments: json!({
                            "project": project,
                            "path": "src/kernel-once.txt",
                            "content": "x\n",
                            "session_id": recording_id,
                        }),
                    },
                    ToolCallContext {
                        transport: ToolTransport::Api,
                        session_id: Some(&recording_id),
                        auth: Some(&bootstrap),
                        window: None,
                        record_oauth_scope_denials: true,
                        host_file_import_trust:
                            crate::tool_runtime::kernel::HostFileImportTrust::Untrusted,
                    },
                )
                .await
        }
    });
    complete_write_ok(&runtime, client_id, "src/kernel-once.txt").await;
    let outcome = task.await.unwrap();
    assert!(
        outcome.success,
        "{:?}",
        outcome.result.as_ref().and_then(|r| r.error.clone())
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "kernel+dispatch must evaluate permission exactly once"
    );

    let result = outcome.result.expect("tool result");
    assert!(
        result.output.get("permission").is_none(),
        "successful kernel result should not repeat permission metadata: {}",
        result.output
    );
    assert!(result.output.get("session_recorded").is_none());
    assert!(result.output.get("session_event_id").is_none());

    let summary = runtime
        .sessions
        .summary(&recording.session_id, Some(40))
        .unwrap();
    let mut seen_ids = Vec::new();
    for event in &summary.events {
        if let Some(perm) = event.permission.as_ref() {
            seen_ids.push(perm.request_id.clone());
        }
    }
    assert!(
        !seen_ids.is_empty(),
        "outer recording session should reuse attached decision"
    );
    assert!(seen_ids.iter().all(|id| id.starts_with("wc_perm_")));
    let unique: std::collections::BTreeSet<_> = seen_ids.iter().collect();
    assert_eq!(
        unique.len(),
        1,
        "duplicate permission request ids: {seen_ids:?}"
    );

    let tool_events = summary
        .events
        .iter()
        .filter(|event| event.kind.starts_with("tool_call_"))
        .collect::<Vec<_>>();
    assert_eq!(
        tool_events.len(),
        4,
        "raw recorder + business pairs must remain"
    );
    let logical_ids = tool_events
        .iter()
        .map(|event| {
            event
                .logical_invocation_id
                .as_deref()
                .expect("logical invocation id")
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        logical_ids.len(),
        1,
        "one real kernel request must share one logical id"
    );
    let call_ids = tool_events
        .iter()
        .map(|event| event.call_id.as_deref().expect("pair call id"))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        call_ids.len(),
        2,
        "outer and inner pairs retain distinct call ids"
    );
    for call_id in call_ids {
        assert_eq!(
            tool_events
                .iter()
                .filter(|event| event.call_id.as_deref() == Some(call_id))
                .count(),
            2,
            "each call id must still correlate exactly one start/finish pair"
        );
    }
    let roles = tool_events
        .iter()
        .map(|event| {
            event
                .logical_invocation_role
                .as_deref()
                .expect("logical role")
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        roles,
        std::collections::BTreeSet::from(["business", "recorder"])
    );
    let (work, _) = super::super::handoff::closeout_work_projection(&summary.events);
    assert_eq!(work.as_array().unwrap().len(), 1);
    assert_eq!(work[0]["tool_name"], "write_project_file");
    assert_eq!(work[0]["count"], 1);
    assert_eq!(summary.counts.tool_calls, 1);
    let permission_summary =
        super::super::permissions::permission_summary_from_events(&summary.events, 10);
    assert_eq!(permission_summary["events_total"], 1);
    assert_eq!(permission_summary["auto_approved_count"], 1);
}

#[tokio::test]
async fn kernel_logical_invocation_correlation_is_session_local_across_recorder_and_business_sessions(
) {
    let counter = Arc::new(AtomicUsize::new(0));
    let client_id = "logical-cross-session";
    let runtime = runtime_with_agent_project(client_id).with_permission_evaluator(
        PermissionEvaluator::with_mode(AuthorityMode::TrustedAgent)
            .with_eval_counter(counter.clone()),
    );
    register_write_agent(&runtime, client_id).await;
    let project = agent_test_project_id(client_id);
    let recorder = runtime
        .sessions
        .start_session(Some(project.clone()), Some("recorder".to_string()));
    let business = runtime
        .sessions
        .start_session(Some(project.clone()), Some("business".to_string()));
    let auth = auth_context(None, true);

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let recorder_id = recorder.session_id.clone();
        let business_id = business.session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .call_tool_with_context(
                    ToolCallRequest {
                        tool_name: "write_project_file".to_string(),
                        arguments: json!({
                            "project": project,
                            "path": "src/logical-cross-session.txt",
                            "content": "x\n",
                            "session_id": business_id,
                        }),
                    },
                    ToolCallContext {
                        transport: ToolTransport::Api,
                        session_id: Some(&recorder_id),
                        auth: Some(&auth),
                        window: None,
                        record_oauth_scope_denials: true,
                        host_file_import_trust:
                            crate::tool_runtime::kernel::HostFileImportTrust::Untrusted,
                    },
                )
                .await
        }
    });
    complete_write_ok(&runtime, client_id, "src/logical-cross-session.txt").await;
    let outcome = task.await.unwrap();
    assert!(
        outcome.success,
        "{:?}",
        outcome.result.as_ref().and_then(|r| r.error.clone())
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    let recorder_summary = runtime
        .sessions
        .summary(&recorder.session_id, Some(20))
        .unwrap();
    let business_summary = runtime
        .sessions
        .summary(&business.session_id, Some(20))
        .unwrap();
    let recorder_events = recorder_summary
        .events
        .iter()
        .filter(|event| event.kind.starts_with("tool_call_"))
        .collect::<Vec<_>>();
    let business_events = business_summary
        .events
        .iter()
        .filter(|event| event.kind.starts_with("tool_call_"))
        .collect::<Vec<_>>();
    assert_eq!(recorder_events.len(), 2);
    assert_eq!(business_events.len(), 2);
    let logical_id = recorder_events[0].logical_invocation_id.as_deref().unwrap();
    assert!(recorder_events
        .iter()
        .all(|event| event.logical_invocation_id.as_deref() == Some(logical_id)));
    assert!(business_events
        .iter()
        .all(|event| event.logical_invocation_id.as_deref() == Some(logical_id)));
    assert!(recorder_events
        .iter()
        .all(|event| event.logical_invocation_role.as_deref() == Some("recorder")));
    assert!(business_events
        .iter()
        .all(|event| event.logical_invocation_role.as_deref() == Some("business")));
    let (recorder_work, _) =
        super::super::handoff::closeout_work_projection(&recorder_summary.events);
    let (business_work, _) =
        super::super::handoff::closeout_work_projection(&business_summary.events);
    assert_eq!(recorder_work[0]["count"], 1);
    assert_eq!(business_work[0]["count"], 1);
    assert_eq!(recorder_summary.counts.tool_calls, 1);
    assert_eq!(business_summary.counts.tool_calls, 1);
}
