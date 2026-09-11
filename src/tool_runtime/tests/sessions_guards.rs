//! Session guard tests: read-only sessions, guard denial, deny_write/deny_shell.

use super::super::*;
use super::support::*;
use crate::runner_protocol::RunnerCapabilities;
use crate::tool_runtime::kernel::{ToolCallContext, ToolCallRequest, ToolTransport};
use serde_json::json;
use std::fs;
use std::path::Path;

async fn runtime_with_two_agent_projects(
    root_a: &Path,
    root_b: &Path,
) -> (ToolRuntime, String, String) {
    let runtime = test_runtime();
    let alpha = register_runner_project_at_path(&runtime, "alpha-client", "alpha", root_a).await;
    let bravo = register_runner_project_at_path(&runtime, "bravo-client", "bravo", root_b).await;
    (runtime, alpha, bravo)
}

fn latest_finished_event<'a>(
    summary: &'a sessions::SessionSummary,
    tool_name: &str,
) -> &'a sessions::SessionEvent {
    summary
        .events
        .iter()
        .rev()
        .find(|event| event.kind == "tool_call_finished" && event.tool_name == tool_name)
        .unwrap_or_else(|| panic!("missing finished event for {tool_name}"))
}

#[tokio::test]
async fn unknown_session_id_fails_before_execution_or_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("README.md"), "hello\n").unwrap();
    let runtime = runtime_with_project(root, "demo");

    let read = runtime
        .dispatch(ToolCall::ReadFiles {
            project: "demo".to_string(),
            items: vec![crate::tool_runtime::ReadFilesItem {
                path: "README.md".to_string(),
                start_line: None,
                limit: None,
            }],
            session_id: Some("wc_sess_missing".to_string()),
            with_line_numbers: None,
            max_result_bytes: None,
        })
        .await;
    assert!(!read.success);
    assert_eq!(read.output["error_kind"], "unknown_session_id");
    assert_eq!(read.output["session_id"], "wc_sess_missing");
    assert!(read.output.get("permission").is_none());
    assert!(read
        .error
        .as_deref()
        .unwrap()
        .contains("unknown_session_id"));

    let write = runtime
        .dispatch(ToolCall::WriteProjectFile {
            project: "demo".to_string(),
            path: "should-not-exist.txt".to_string(),
            content: "nope".to_string(),
            session_id: Some("wc_sess_missing".to_string()),
            overwrite: None,
            expected_sha256: None,
        })
        .await;
    assert!(!write.success);
    assert_eq!(write.output["error_kind"], "unknown_session_id");
    assert!(write.output.get("permission").is_none());
    assert!(!root.join("should-not-exist.txt").exists());
}

#[tokio::test]
async fn same_project_session_records_without_project_mismatch_warning() {
    let tmp_a = tempfile::tempdir().unwrap();
    let tmp_b = tempfile::tempdir().unwrap();
    fs::write(tmp_a.path().join("README.md"), "alpha\n").unwrap();
    fs::write(tmp_b.path().join("README.md"), "bravo\n").unwrap();
    let (runtime, alpha, _bravo) =
        runtime_with_two_agent_projects(tmp_a.path(), tmp_b.path()).await;
    let auth = auth_context(None, true);
    let session = runtime
        .sessions
        .start_session(Some(alpha.clone()), Some("same".to_string()));

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let alpha = alpha.clone();
        let session_id = session.session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFiles {
                        project: alpha,
                        items: vec![crate::tool_runtime::ReadFilesItem {
                            path: "README.md".to_string(),
                            start_line: None,
                            limit: None,
                        }],
                        session_id: Some(session_id),
                        with_line_numbers: None,
                        max_result_bytes: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "alpha-client").await;
    complete_patch_agent_request(
        &runtime,
        "alpha-client",
        &req.request_id,
        0,
        &canonical_agent_file_read_output("alpha\n", 1),
        "",
    )
    .await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert!(result.output.get("warning_kind").is_none());
    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    let event = latest_finished_event(&summary, "read_files");
    assert!(event.warning_kind.is_none());
}

#[tokio::test]
async fn read_only_cross_project_session_is_blocked_before_execution() {
    let tmp_a = tempfile::tempdir().unwrap();
    let tmp_b = tempfile::tempdir().unwrap();
    fs::write(tmp_a.path().join("README.md"), "alpha\n").unwrap();
    fs::write(tmp_b.path().join("README.md"), "bravo\n").unwrap();
    let (runtime, alpha, bravo) = runtime_with_two_agent_projects(tmp_a.path(), tmp_b.path()).await;
    let auth = auth_context(None, true);
    let session = runtime
        .sessions
        .start_session(Some(alpha.clone()), Some("read mismatch".to_string()));

    let result = runtime
        .dispatch_with_auth(
            ToolCall::ReadFiles {
                project: bravo.clone(),
                items: vec![crate::tool_runtime::ReadFilesItem {
                    path: "README.md".to_string(),
                    start_line: None,
                    limit: None,
                }],
                session_id: Some(session.session_id.clone()),
                with_line_numbers: None,
                max_result_bytes: None,
            },
            Some(&auth),
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "session_project_mismatch");
    assert_eq!(result.output["failure_kind"], "session_project_mismatch");
    assert_eq!(result.output["session_project"], alpha);
    assert_eq!(result.output["request_project"], bravo);
    assert_eq!(result.output["command_started"], false);
    assert_eq!(result.output["state_changed"], false);
    assert!(result.output.get("permission").is_none());
    assert!(
        probe_agent_request_for_instance(&runtime, "bravo-client", "inst")
            .await
            .is_none(),
        "read mismatch must fail before an agent request is enqueued"
    );
}

#[tokio::test]
async fn mutation_cross_project_session_fails_before_write() {
    let tmp_a = tempfile::tempdir().unwrap();
    let tmp_b = tempfile::tempdir().unwrap();
    let (runtime, alpha, bravo) = runtime_with_two_agent_projects(tmp_a.path(), tmp_b.path()).await;
    let auth = auth_context(None, true);
    let session = runtime
        .sessions
        .start_session(Some(alpha.clone()), Some("write mismatch".to_string()));

    let result = runtime
        .dispatch_with_auth(
            ToolCall::WriteProjectFile {
                project: bravo.clone(),
                path: "blocked.txt".to_string(),
                content: "nope\n".to_string(),
                session_id: Some(session.session_id.clone()),
                overwrite: None,
                expected_sha256: None,
            },
            Some(&auth),
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.output["failure_kind"], "session_project_mismatch");
    assert_eq!(result.output["session_project"], alpha);
    assert_eq!(result.output["request_project"], bravo);
    assert!(result
        .output
        .get("allow_cross_project_session_required")
        .is_none());
    assert!(result.output.get("allow_cross_project_session").is_none());
    assert_eq!(result.output["command_started"], false);
    assert!(result.output.get("permission").is_none());
    assert!(!tmp_b.path().join("blocked.txt").exists());

    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    let event = latest_finished_event(&summary, "write_project_file");
    assert_eq!(
        event.failure_kind.as_deref(),
        Some("session_project_mismatch")
    );
    assert_eq!(
        event.error_kind.as_deref(),
        Some("session_project_mismatch")
    );
    assert!(event.permission.is_none());
}

#[tokio::test]
async fn read_only_recording_session_does_not_guard_same_project_write() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "guard-recorder", "demo", tmp.path()).await;
    let auth = auth_context(None, true);
    let recorder = runtime.sessions.start_session_with_guards(
        Some(project.clone()),
        Some("read only recorder".to_string()),
        SessionMode::ReadOnly,
        sessions::SessionGuards::default(),
    );

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let recorder_id = recorder.session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .call_tool_with_context(
                    ToolCallRequest {
                        tool_name: "write_project_file".to_string(),
                        arguments: json!({
                            "project": project,
                            "path": "recorder-does-not-guard.txt",
                            "content": "allowed\n"
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
    let request = wait_for_patch_agent_request(&runtime, "guard-recorder").await;
    assert_eq!(request.kind, "file_write_project_file");
    let payload: serde_json::Value =
        serde_json::from_str(request.content.as_deref().expect("file-op payload")).unwrap();
    assert_eq!(payload["path"], "recorder-does-not-guard.txt");
    assert_eq!(payload["content"], "allowed\n");
    complete_patch_agent_request(
        &runtime,
        "guard-recorder",
        &request.request_id,
        0,
        r#"{"path":"recorder-does-not-guard.txt","bytes_written":8,"sha256":"abc","changed":true,"state_changed":true,"execution_state":"completed"}"#,
        "",
    )
    .await;
    let outcome = task.await.unwrap();

    assert!(outcome.success, "{:?}", outcome.result);
    let output = &outcome.result.as_ref().unwrap().output;
    assert_ne!(output["error_kind"], "session_guard_denied");
    let serialized_output = serde_json::to_string(output).unwrap();
    assert!(!serialized_output.contains("recording_session_project"));
    assert!(!serialized_output.contains("recording_session_authorized"));

    let summary = runtime
        .sessions
        .summary(&recorder.session_id, Some(20))
        .unwrap();
    let event = latest_finished_event(&summary, "write_project_file");
    assert_eq!(event.status.as_deref(), Some("succeeded"));
    assert_ne!(event.error_kind.as_deref(), Some("session_guard_denied"));
    let serialized_events = serde_json::to_string(&summary.events).unwrap();
    assert!(!serialized_events.contains("recording_session_project"));
    assert!(!serialized_events.contains("recording_session_authorized"));
}

#[tokio::test]
async fn closed_recording_session_remains_provenance_only_for_business_write() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "guard-closed-recorder", "demo", tmp.path())
            .await;
    let auth = auth_context(None, true);
    let recorder = runtime
        .sessions
        .start_session(Some(project.clone()), Some("closed recorder".to_string()));
    runtime
        .sessions
        .close_session(&recorder.session_id)
        .unwrap();

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let recorder_id = recorder.session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .call_tool_with_context(
                    ToolCallRequest {
                        tool_name: "write_project_file".to_string(),
                        arguments: json!({
                            "project": project,
                            "path": "closed-recorder-write.txt",
                            "content": "allowed\n"
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
    let request = wait_for_patch_agent_request(&runtime, "guard-closed-recorder").await;
    assert_eq!(request.kind, "file_write_project_file");
    complete_patch_agent_request(
        &runtime,
        "guard-closed-recorder",
        &request.request_id,
        0,
        r#"{"path":"closed-recorder-write.txt","bytes_written":8,"sha256":"abc","changed":true,"state_changed":true,"execution_state":"completed"}"#,
        "",
    )
    .await;
    let outcome = task.await.unwrap();

    assert!(outcome.success, "{:?}", outcome.result);
    let summary = runtime
        .sessions
        .summary(&recorder.session_id, Some(50))
        .unwrap();
    assert_eq!(summary.lifecycle, sessions::SessionLifecycle::Closed);
    assert_eq!(
        latest_finished_event(&summary, "write_project_file")
            .status
            .as_deref(),
        Some("succeeded")
    );
}

#[tokio::test]
async fn recording_session_id_obeys_project_boundary() {
    let tmp_a = tempfile::tempdir().unwrap();
    let tmp_b = tempfile::tempdir().unwrap();
    let (runtime, alpha, bravo) = runtime_with_two_agent_projects(tmp_a.path(), tmp_b.path()).await;
    let auth = auth_context(None, true);
    let session = runtime
        .sessions
        .start_session(Some(alpha.clone()), Some("record mismatch".to_string()));

    let outcome = runtime
        .call_tool_with_context(
            ToolCallRequest {
                tool_name: "write_project_file".to_string(),
                arguments: json!({
                    "project": bravo.clone(),
                    "path": "recording-blocked.txt",
                    "content": "nope\n"
                }),
            },
            ToolCallContext {
                transport: ToolTransport::Api,
                session_id: Some(&session.session_id),
                auth: Some(&auth),
                window: None,
                record_oauth_scope_denials: true,
                host_file_import_trust: crate::tool_runtime::kernel::HostFileImportTrust::Untrusted,
            },
        )
        .await;

    assert!(!outcome.success);
    let result = outcome.result.unwrap();
    assert_eq!(result.output["failure_kind"], "session_project_mismatch");
    assert_eq!(result.output["command_started"], false);
    assert!(result.output.get("permission").is_none());
    assert!(!tmp_b.path().join("recording-blocked.txt").exists());
    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    let event = latest_finished_event(&summary, "write_project_file");
    assert_eq!(
        event.failure_kind.as_deref(),
        Some("session_project_mismatch")
    );
    assert_eq!(event.request_project.as_deref(), Some(bravo.as_str()));
    assert!(event.permission.is_none());
}

#[tokio::test]
async fn start_session_mode_effective_guards_matrix() {
    for (case, input, expected_mode, deny_write_tools, deny_shell_tools) in [
        (
            "normal defaults",
            json!({}),
            SessionMode::Normal,
            false,
            false,
        ),
        (
            "read_only fixed profile overrides caller shell guard",
            json!({"mode": "read_only", "deny_shell_tools": false}),
            SessionMode::ReadOnly,
            true,
            true,
        ),
    ] {
        let runtime = test_runtime();
        let result = runtime
            .dispatch(ToolCall::from_tool_name("start_session", input).unwrap())
            .await;

        assert!(result.success, "{case}: {:?}", result.error);
        assert_eq!(result.output["mode"], expected_mode.as_str(), "{case}");
        assert_eq!(
            result.output["guards"]["deny_write_tools"], deny_write_tools,
            "{case}"
        );
        assert_eq!(
            result.output["guards"]["deny_shell_tools"], deny_shell_tools,
            "{case}"
        );

        let session_id = result.output["session_id"].as_str().unwrap();
        let summary = runtime.sessions.summary(session_id, None).unwrap();
        assert_eq!(summary.mode, expected_mode, "{case}");
        assert_eq!(summary.guards.deny_write_tools, deny_write_tools, "{case}");
        assert_eq!(summary.guards.deny_shell_tools, deny_shell_tools, "{case}");
    }
}

#[tokio::test]
async fn read_only_session_allows_read_files_and_records_success() {
    let runtime = runtime_with_agent_project("guard-read");
    register_agent(
        &runtime,
        "guard-read",
        None,
        RunnerCapabilities {
            file_read: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("guard-read");
    let session = runtime.sessions.start_session_with_guards(
        Some(project.clone()),
        Some("read only".to_string()),
        SessionMode::ReadOnly,
        sessions::SessionGuards::default(),
    );

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFiles {
                        project: project,
                        items: vec![crate::tool_runtime::ReadFilesItem {
                            path: "README.md".to_string(),
                            start_line: None,
                            limit: None,
                        }],
                        session_id: Some(session_id),
                        with_line_numbers: None,
                        max_result_bytes: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_runner_request_for_instance(&runtime, "guard-read", "inst").await;
    assert_eq!(req.kind, "file_read");
    complete_patch_agent_request(
        &runtime,
        "guard-read",
        &req.request_id,
        0,
        &canonical_agent_file_read_output("hello\n", 1),
        "",
    )
    .await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert!(result.output.get("session_recorded").is_none());
    assert!(result.output.get("session_event_id").is_none());
    assert!(result.output.get("session_id").is_none());
    assert!(result.output.get("permission").is_none());
    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    assert_eq!(summary.counts.succeeded, 1);
    assert_eq!(summary.counts.read_like, 1);
    assert_eq!(
        finished_event(&summary, "read_files").status.as_deref(),
        Some("succeeded")
    );
    assert!(finished_event(&summary, "read_files").permission.is_none());

    let handoff = runtime
        .dispatch(ToolCall::SessionHandoffSummary {
            session_id: session.session_id.clone(),
            project: None,
            include_workspace: Some(false),
            include_checkpoints: Some(false),
            include_validation: Some(false),
            summary_only: false,
            limit: None,
        })
        .await;
    assert!(handoff.success, "{:?}", handoff.error);
    assert_eq!(handoff.output["permissions"]["required_count"], 0);
    assert_eq!(handoff.output["permissions"]["auto_approved_count"], 0);
}

#[tokio::test]
async fn read_only_session_rejects_write_project_file_before_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime = runtime_with_project(tmp.path(), "demo");
    let session = runtime.sessions.start_session_with_guards(
        Some("demo".to_string()),
        Some("read only".to_string()),
        SessionMode::ReadOnly,
        sessions::SessionGuards::default(),
    );
    let message_text = "guard risk message must stay out of hint";
    runtime
        .sessions
        .post_message(sessions::PostSessionMessageInput {
            session_id: session.session_id.clone(),
            kind: sessions::SessionMessageKind::Risk,
            message: message_text.to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: sessions::SessionMessagePriority::High,
        })
        .unwrap();

    let result = runtime
        .dispatch(ToolCall::WriteProjectFile {
            project: "demo".to_string(),
            path: "should-not-exist.txt".to_string(),
            content: "nope".to_string(),
            session_id: Some(session.session_id.clone()),
            overwrite: None,
            expected_sha256: None,
        })
        .await;

    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "session_guard_denied");
    assert_eq!(result.output["guard"], "deny_write_tools");
    assert_eq!(result.output["mode"], "read_only");
    assert!(result.output.get("permission").is_none());
    assert!(result.output.get("session_recorded").is_none());
    assert!(result.output.get("session_event_id").is_none());
    assert_eq!(result.output["session_id"], session.session_id);
    assert_eq!(result.output["session_hint"]["has_open_messages"], true);
    assert_eq!(result.output["session_hint"]["open_counts"]["risk"], 1);
    assert_eq!(result.output["session_hint"]["highest_priority"], "high");
    assert!(result.output["session_hint"]
        .get("attention_required")
        .is_none());
    assert!(result.output["session_hint"]
        .get("attention_reason")
        .is_none());
    assert!(result.output["session_hint"]
        .get("attention_instruction")
        .is_none());
    let serialized_hint = serde_json::to_string(&result.output["session_hint"]).unwrap();
    assert!(
        !serialized_hint.contains(message_text),
        "session_hint leaked message text: {serialized_hint}"
    );
    assert!(!tmp.path().join("should-not-exist.txt").exists());
    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    assert_eq!(summary.counts.failed, 1);
    assert_eq!(summary.counts.write_like, 1);
    let event = finished_event(&summary, "write_project_file");
    assert_eq!(event.status.as_deref(), Some("failed"));
    assert_eq!(event.error_kind.as_deref(), Some("session_guard_denied"));
    assert!(event.permission.is_none());
}

#[tokio::test]
async fn read_only_session_rejects_all_artifact_upload_tools_without_base64_leak() {
    let runtime = runtime_with_agent_project("guard-artifact-upload");
    register_agent(
        &runtime,
        "guard-artifact-upload",
        None,
        RunnerCapabilities {
            file_write: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("guard-artifact-upload");
    let session = runtime.sessions.start_session_with_guards(
        Some(project.clone()),
        Some("read only artifacts".to_string()),
        SessionMode::ReadOnly,
        sessions::SessionGuards::default(),
    );
    let content_base64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        "SECRET_UPLOAD_CHUNK_DO_NOT_LOG",
    );
    let bootstrap = auth_context(None, true);

    let calls = vec![
        ToolCall::ArtifactUploadBegin {
            project: project.clone(),
            path: "artifacts/imports/blocked.txt".to_string(),
            session_id: Some(session.session_id.clone()),
            expected_bytes: Some(3),
            expected_sha256: None,
            mime_type: Some("text/plain".to_string()),
            overwrite: None,
        },
        ToolCall::ArtifactUploadChunk {
            project: project.clone(),
            path: "artifacts/imports/blocked.txt".to_string(),
            upload_id: "wc_upload_test_1".to_string(),
            offset: 0,
            content_base64: content_base64.clone(),
            session_id: Some(session.session_id.clone()),
        },
        ToolCall::ArtifactUploadFinish {
            project: project.clone(),
            path: "artifacts/imports/blocked.txt".to_string(),
            upload_id: "wc_upload_test_1".to_string(),
            session_id: Some(session.session_id.clone()),
        },
        ToolCall::ArtifactUploadAbort {
            project,
            path: "artifacts/imports/blocked.txt".to_string(),
            upload_id: "wc_upload_test_1".to_string(),
            session_id: Some(session.session_id.clone()),
        },
    ];

    for call in calls {
        let tool_name = call.tool_name().to_string();
        let result = runtime.dispatch_with_auth(call, Some(&bootstrap)).await;
        assert!(!result.success, "{tool_name}");
        assert_eq!(result.output["error_kind"], "session_guard_denied");
        assert_eq!(result.output["guard"], "deny_write_tools");
        assert_eq!(result.output["mode"], "read_only");
        assert!(
            result.output.get("session_recorded").is_none(),
            "{tool_name}"
        );
        assert!(
            result.output.get("session_event_id").is_none(),
            "{tool_name}"
        );
        assert_eq!(
            result.output["session_id"], session.session_id,
            "{tool_name}"
        );
    }
    assert!(
        probe_patch_agent_request(&runtime, "guard-artifact-upload")
            .await
            .is_none(),
        "artifact upload guard denial must not enqueue an agent request"
    );

    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    assert_eq!(summary.counts.failed, 4);
    assert_eq!(summary.counts.write_like, 4);
    for tool_name in [
        "artifact_upload_begin",
        "artifact_upload_chunk",
        "artifact_upload_finish",
        "artifact_upload_abort",
    ] {
        let event = finished_event(&summary, tool_name);
        assert_eq!(event.status.as_deref(), Some("failed"), "{tool_name}");
        assert_eq!(
            event.error_kind.as_deref(),
            Some("session_guard_denied"),
            "{tool_name}"
        );
    }

    let started = summary
        .events
        .iter()
        .rev()
        .find(|event| {
            event.kind == "tool_call_started" && event.tool_name == "artifact_upload_chunk"
        })
        .expect("started event for artifact_upload_chunk");
    let input_summary = started.input_summary.as_ref().unwrap();
    assert_eq!(input_summary["path"], "artifacts/imports/blocked.txt");
    assert_eq!(input_summary["upload_id"], "wc_upload_test_1");
    assert_eq!(input_summary["offset"], 0);
    assert_eq!(input_summary["content_base64_present"], true);
    assert!(input_summary.get("content_base64").is_none());
    let serialized = serde_json::to_string(&summary.events).unwrap();
    assert!(
        !serialized.contains(&content_base64) && !serialized.contains("SECRET_UPLOAD_CHUNK"),
        "guard denial event leaked artifact chunk content: {serialized}"
    );
}

#[tokio::test]
async fn read_only_session_rejects_run_shell_before_agent_enqueue() {
    let runtime = runtime_with_agent_project("guard-shell");
    register_agent(
        &runtime,
        "guard-shell",
        None,
        RunnerCapabilities {
            shell: true,
            file_write: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("guard-shell");
    let session = runtime.sessions.start_session_with_guards(
        Some(project.clone()),
        Some("read only".to_string()),
        SessionMode::ReadOnly,
        sessions::SessionGuards::default(),
    );

    let bootstrap = auth_context(None, true);
    let result = runtime
        .dispatch_with_auth(
            ToolCall::RunShell {
                project,
                command: "echo should-not-run".to_string(),
                session_id: Some(session.session_id.clone()),
                timeout_secs: Some(30),
                cwd: None,
                purpose: None,
                shell: None,
            },
            Some(&bootstrap),
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "session_guard_denied");
    assert_eq!(result.output["guard"], "deny_shell_tools");
    assert_eq!(result.output["command_started"], false);
    assert!(result.output.get("permission").is_none());
    assert!(result.output.get("session_recorded").is_none());
    assert!(result.output.get("session_event_id").is_none());
    assert_eq!(result.output["session_id"], session.session_id);
    assert!(
        probe_patch_agent_request(&runtime, "guard-shell")
            .await
            .is_none(),
        "run_shell guard denial must not enqueue an agent request"
    );
    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    assert_eq!(summary.counts.failed, 1);
    assert_eq!(summary.counts.shell_like, 1);
    let event = finished_event(&summary, "run_shell");
    assert_eq!(event.error_kind.as_deref(), Some("session_guard_denied"));
    assert!(event.permission.is_none());
}

#[tokio::test]
async fn deny_write_only_allows_read_and_shell_tools() {
    let runtime = runtime_with_agent_project("guard-write-only");
    register_agent(
        &runtime,
        "guard-write-only",
        None,
        RunnerCapabilities {
            file_read: true,
            shell: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("guard-write-only");
    let session = runtime.sessions.start_session_with_guards(
        Some(project.clone()),
        None,
        SessionMode::Normal,
        sessions::SessionGuards {
            deny_write_tools: true,
            deny_shell_tools: false,
        },
    );
    let bootstrap = auth_context(None, true);

    let denied = runtime
        .dispatch_with_auth(
            ToolCall::WriteProjectFile {
                project: project.clone(),
                path: "blocked.txt".to_string(),
                content: "x".to_string(),
                session_id: Some(session.session_id.clone()),
                overwrite: None,
                expected_sha256: None,
            },
            Some(&bootstrap),
        )
        .await;
    assert!(!denied.success);
    assert_eq!(denied.output["guard"], "deny_write_tools");

    let read_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFiles {
                        project: project,
                        items: vec![crate::tool_runtime::ReadFilesItem {
                            path: "README.md".to_string(),
                            start_line: None,
                            limit: None,
                        }],
                        session_id: Some(session_id),
                        with_line_numbers: None,
                        max_result_bytes: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_runner_request_for_instance(&runtime, "guard-write-only", "inst").await;
    complete_patch_agent_request(
        &runtime,
        "guard-write-only",
        &req.request_id,
        0,
        &canonical_agent_file_read_output("hello\n", 1),
        "",
    )
    .await;
    assert!(read_task.await.unwrap().success);

    let shell_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::RunShell {
                        project,
                        command: "exit 0".to_string(),
                        session_id: Some(session_id),
                        timeout_secs: Some(30),
                        cwd: None,
                        purpose: None,
                        shell: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "guard-write-only").await;
    complete_patch_agent_request(&runtime, "guard-write-only", &req.request_id, 0, "", "").await;
    assert!(shell_task.await.unwrap().success);
}

#[tokio::test]
async fn deny_shell_only_allows_write_tools() {
    let runtime = runtime_with_agent_project("guard-shell-only");
    register_agent(
        &runtime,
        "guard-shell-only",
        None,
        RunnerCapabilities {
            shell: true,
            file_write: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("guard-shell-only");
    let session = runtime.sessions.start_session_with_guards(
        Some(project.clone()),
        None,
        SessionMode::Normal,
        sessions::SessionGuards {
            deny_write_tools: false,
            deny_shell_tools: true,
        },
    );
    let bootstrap = auth_context(None, true);

    let denied = runtime
        .dispatch_with_auth(
            ToolCall::RunShell {
                project: project.clone(),
                command: "echo blocked".to_string(),
                session_id: Some(session.session_id.clone()),
                timeout_secs: Some(30),
                cwd: None,
                purpose: None,
                shell: None,
            },
            Some(&bootstrap),
        )
        .await;
    assert!(!denied.success);
    assert_eq!(denied.output["guard"], "deny_shell_tools");

    let write_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::WriteProjectFile {
                        project,
                        path: "allowed.txt".to_string(),
                        content: "x".to_string(),
                        session_id: Some(session_id),
                        overwrite: None,
                        expected_sha256: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "guard-shell-only").await;
    assert_eq!(req.kind, "file_write_project_file");
    complete_patch_agent_request(
        &runtime,
        "guard-shell-only",
        &req.request_id,
        0,
        r#"{"path":"allowed.txt","created":true,"overwritten":false,"bytes_written":1,"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","changed":true,"state_changed":true,"execution_state":"completed"}"#,
        "",
    )
    .await;
    assert!(write_task.await.unwrap().success);
}

#[test]
fn project_tool_schemas_include_optional_session_id() {
    let specs = registered_tool_specs();
    // `start_session` is ModelHidden (the model coding line is covered by
    // `work_on_project`): it has no public ToolSpec, so its guard schema is
    // verified directly from the output schema builder rather than via
    // `spec_named`, keeping the session guard-field contract asserted at the
    // implementation level.
    let start_session_output =
        crate::tool_runtime::registry::output_schema_for_tool("start_session");
    assert!(start_session_output["properties"]["output"]["properties"]
        .get("mode")
        .is_some());
    assert!(start_session_output["properties"]["output"]["properties"]
        .get("guards")
        .is_some());
    assert!(start_session_output["properties"]["output"]["properties"]
        .get("execution_context")
        .is_some());
    assert!(start_session_output["properties"]["output"]["properties"]
        .get("lifecycle")
        .is_some());
    let session_summary = spec_named(&specs, "session_summary");
    assert!(
        session_summary.output_schema["properties"]["output"]["properties"]
            .get("mode")
            .is_some()
    );
    assert!(
        session_summary.output_schema["properties"]["output"]["properties"]
            .get("guards")
            .is_some()
    );
    assert!(
        session_summary.output_schema["properties"]["output"]["properties"]
            .get("execution_context")
            .is_some()
    );
    assert!(
        session_summary.output_schema["properties"]["output"]["properties"]
            .get("lifecycle")
            .is_some()
    );
    for name in [
        "read_files",
        "run_shell",
        "write_project_file",
        "git_status",
        "git_log",
        "show_changes",
    ] {
        let spec = spec_named(&specs, name);
        assert!(
            spec.input_schema["properties"].get("session_id").is_some(),
            "{name} schema missing session_id"
        );
        assert!(
            spec.input_schema["properties"]
                .get("allow_cross_project_session")
                .is_none(),
            "{name} model-facing schema must hide the cross-project debug escape"
        );
        assert!(
            !spec.input_schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == "session_id"),
            "{name} schema must not require session_id"
        );
    }
    let assert_session_hint_contract = |session_hint: &serde_json::Value| {
        assert_eq!(session_hint["type"], "object");
        assert_eq!(
            session_hint["properties"]["suggested_next_tool"]["enum"],
            json!(["session_discussion_summary"])
        );
        assert_eq!(
            session_hint["properties"]["attention_required"]["const"],
            true
        );
        assert_eq!(
            session_hint["properties"]["attention_reason"]["enum"],
            json!(["high_priority_guidance_requires_ack"])
        );
        assert_eq!(
            session_hint["properties"]["attention_instruction"]["enum"],
            json!(["High-priority Session guidance is pending. Read session_discussion_summary before continuing."])
        );
        for optional in [
            "attention_required",
            "attention_reason",
            "attention_instruction",
        ] {
            assert!(!session_hint["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == optional));
        }
    };

    let read_files = spec_named(&specs, "read_files");
    let read_files_outputs = read_files.output_schema["properties"]["output"]["anyOf"][0]["anyOf"]
        .as_array()
        .expect("read_files success output variants");
    for output in read_files_outputs {
        let properties = output["properties"]
            .as_object()
            .expect("read_files output properties");
        for recorder_only in ["session_recorded", "session_event_id", "session_id"] {
            assert!(properties.get(recorder_only).is_none());
        }
        assert_session_hint_contract(&output["properties"]["session_hint"]);
    }

    for name in ["run_shell", "write_project_file"] {
        let spec = spec_named(&specs, name);
        let properties = &spec.output_schema["properties"]["output"]["properties"];
        assert!(properties.get("session_recorded").is_none());
        assert!(properties.get("session_event_id").is_none());
        assert!(properties.get("session_id").is_none());
        assert_session_hint_contract(&properties["session_hint"]);
    }
}
