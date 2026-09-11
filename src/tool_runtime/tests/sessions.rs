//! Core session lifecycle and summary tests.

use super::super::sessions::{SessionMessageKind, SessionMessagePriority};
use super::super::*;
use super::super::{ToolCall, ToolResult, ToolRuntime};
use super::support::*;
use crate::runner_protocol::RunnerCapabilities;
use serde_json::Value;

async fn post_session_message(
    runtime: &ToolRuntime,
    session_id: &str,
    kind: SessionMessageKind,
    priority: SessionMessagePriority,
    message: &str,
) -> String {
    let result = runtime
        .dispatch(ToolCall::PostSessionMessage {
            session_id: session_id.to_string(),
            kind,
            message: message.to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority,
            requires_ack: false,
        })
        .await;
    assert!(result.success, "{:?}", result.error);
    result.output["message_id"].as_str().unwrap().to_string()
}

async fn read_agent_file_for_session(
    runtime: &ToolRuntime,
    client_id: &str,
    project: String,
    session_id: Option<String>,
) -> ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
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
                        session_id: session_id,
                        with_line_numbers: None,
                        max_result_bytes: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_runner_request_for_instance(runtime, client_id, "inst").await;
    complete_patch_agent_request(
        runtime,
        client_id,
        &req.request_id,
        0,
        &canonical_agent_file_read_output("hello\n", 1),
        "",
    )
    .await;
    task.await.unwrap()
}

#[tokio::test]
async fn read_files_with_session_id_records_event_without_content() {
    let runtime = runtime_with_agent_project("telemetry-read");
    register_agent(
        &runtime,
        "telemetry-read",
        None,
        RunnerCapabilities {
            file_read: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("telemetry-read");
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("read telemetry".to_string()));
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
                        with_line_numbers: Some(true),
                        max_result_bytes: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_runner_request_for_instance(&runtime, "telemetry-read", "inst").await;
    assert_eq!(req.kind, "file_read");
    complete_patch_agent_request(
        &runtime,
        "telemetry-read",
        &req.request_id,
        0,
        &canonical_agent_file_read_output("secret line\nsecond\n", 2),
        "",
    )
    .await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert!(result.output.get("session_recorded").is_none());
    assert!(result.output.get("session_event_id").is_none());
    assert!(result.output.get("session_id").is_none());
    assert!(result.output.get("session_hint").is_none());
    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    assert_eq!(summary.counts.tool_calls, 1);
    assert_eq!(summary.counts.succeeded, 1);
    assert_eq!(summary.counts.read_like, 1);
    let event = finished_event(&summary, "read_files");
    assert_eq!(event.status.as_deref(), Some("succeeded"));
    assert!(event.read_like);
    assert!(!event.write_like);
    let serialized = serde_json::to_string(&summary.events).unwrap();
    assert!(
        !serialized.contains("secret line"),
        "session event leaked read_files content: {serialized}"
    );
}

#[tokio::test]
async fn read_files_without_session_id_omits_session_telemetry() {
    let runtime = runtime_with_agent_project("telemetry-nosession");
    register_agent(
        &runtime,
        "telemetry-nosession",
        None,
        RunnerCapabilities {
            file_read: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("telemetry-nosession");
    let task = tokio::spawn({
        let runtime = runtime.clone();
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
                        session_id: None,
                        with_line_numbers: None,
                        max_result_bytes: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_runner_request_for_instance(&runtime, "telemetry-nosession", "inst").await;
    complete_patch_agent_request(
        &runtime,
        "telemetry-nosession",
        &req.request_id,
        0,
        &canonical_agent_file_read_output("hello\n", 1),
        "",
    )
    .await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["items"][0]["output"]["text"], "hello");
    assert!(result.output["items"][0]["output"].get("format").is_none());
    assert!(result.output.get("session_recorded").is_none());
    assert!(result.output.get("session_hint").is_none());
}

#[tokio::test]
async fn session_inbox_hint_reports_open_guidance_without_text() {
    let client_id = "inbox-guidance";
    let runtime = runtime_with_agent_project(client_id);
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
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("hint".to_string()));
    let message_text = "sensitive guidance body should not be in the hint";
    post_session_message(
        &runtime,
        &session.session_id,
        SessionMessageKind::Guidance,
        SessionMessagePriority::High,
        message_text,
    )
    .await;

    let result = read_agent_file_for_session(
        &runtime,
        client_id,
        project,
        Some(session.session_id.clone()),
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    let hint = &result.output["session_hint"];
    assert_eq!(hint["has_open_messages"], true);
    assert_eq!(hint["open_counts"]["guidance"], 1);
    assert_eq!(hint["open_counts"]["question"], 0);
    assert_eq!(hint["open_counts"]["todo"], 0);
    assert_eq!(hint["open_counts"]["risk"], 0);
    assert_eq!(hint["highest_priority"], "high");
    assert_eq!(hint["suggested_next_tool"], "session_discussion_summary");
    assert!(hint.get("attention_required").is_none());
    assert!(hint.get("attention_reason").is_none());
    assert!(hint.get("attention_instruction").is_none());
    let serialized_hint = serde_json::to_string(hint).unwrap();
    assert!(
        !serialized_hint.contains(message_text),
        "session_hint leaked message text: {serialized_hint}"
    );
}

#[tokio::test]
async fn high_ack_guidance_hint_is_actionable_and_inner_recorder_projects_attention_body() {
    let client_id = "inbox-ack-guidance";
    let runtime = runtime_with_agent_project(client_id);
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
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("ack guidance hint".to_string()));
    let message_text = "read this high-priority guidance before continuing";
    let guidance = runtime
        .sessions
        .post_message_with_ack(
            super::super::sessions::PostSessionMessageInput {
                session_id: session.session_id.clone(),
                kind: SessionMessageKind::Guidance,
                message: message_text.to_string(),
                tags: Vec::new(),
                reply_to: None,
                priority: SessionMessagePriority::High,
            },
            true,
        )
        .unwrap();

    let raw_hint =
        serde_json::to_value(runtime.sessions.inbox_hint(&session.session_id).unwrap()).unwrap();
    assert_eq!(raw_hint["attention_required"], true);
    assert_eq!(
        raw_hint["attention_reason"],
        "high_priority_guidance_requires_ack"
    );
    assert_eq!(
        raw_hint["attention_instruction"],
        "High-priority Session guidance is pending. Read session_discussion_summary before continuing."
    );

    // No outer recording_session_id is supplied here. The existing authorized
    // concrete Session recorder path must still project model-facing attention.
    let result = read_agent_file_for_session(
        &runtime,
        client_id,
        project,
        Some(session.session_id.clone()),
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    let hint = &result.output["session_hint"];
    assert_eq!(hint["suggested_next_tool"], "session_discussion_summary");
    assert!(hint.get("attention_required").is_none());
    assert!(hint.get("attention_reason").is_none());
    assert!(hint.get("attention_instruction").is_none());
    let serialized_hint = serde_json::to_string(hint).unwrap();
    assert!(!serialized_hint.contains(message_text));

    let attention = &result.output["session_attention"];
    assert_eq!(attention["requires_ack"], true);
    assert_eq!(attention["messages"][0]["message_id"], guidance.message_id);
    assert_eq!(attention["messages"][0]["message"], message_text);
    assert_eq!(attention["ack"]["accepted_count"], 0);
    assert_eq!(attention["ack"]["ignored_count"], 0);
}

#[test]
fn outer_session_telemetry_hint_clears_stale_inner_recorder_hint() {
    let runtime = test_runtime();
    let inner = runtime
        .sessions
        .start_session(None, Some("inner hint".to_string()));
    let outer = runtime
        .sessions
        .start_session(None, Some("outer recorder".to_string()));
    runtime
        .sessions
        .post_message_with_ack(
            super::super::sessions::PostSessionMessageInput {
                session_id: inner.session_id.clone(),
                kind: SessionMessageKind::Guidance,
                message: "inner urgent guidance".to_string(),
                tags: Vec::new(),
                reply_to: None,
                priority: SessionMessagePriority::High,
            },
            true,
        )
        .unwrap();

    let mut result = ToolResult::ok(serde_json::json!({}));
    super::super::add_session_hint(&mut result, &runtime.sessions, &inner.session_id);
    assert_eq!(result.output["session_hint"]["attention_required"], true);

    super::super::add_session_hint(&mut result, &runtime.sessions, &outer.session_id);
    assert!(result.output.get("session_hint").is_none());
}

#[tokio::test]
async fn session_inbox_hint_counts_question_todo_and_risk() {
    let client_id = "inbox-counts";
    let runtime = runtime_with_agent_project(client_id);
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
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("counts".to_string()));
    post_session_message(
        &runtime,
        &session.session_id,
        SessionMessageKind::Todo,
        SessionMessagePriority::Low,
        "todo body",
    )
    .await;
    post_session_message(
        &runtime,
        &session.session_id,
        SessionMessageKind::Question,
        SessionMessagePriority::Normal,
        "question body",
    )
    .await;
    post_session_message(
        &runtime,
        &session.session_id,
        SessionMessageKind::Risk,
        SessionMessagePriority::Low,
        "risk body",
    )
    .await;
    post_session_message(
        &runtime,
        &session.session_id,
        SessionMessageKind::Note,
        SessionMessagePriority::High,
        "note body must not affect hint",
    )
    .await;

    let result = read_agent_file_for_session(
        &runtime,
        client_id,
        project,
        Some(session.session_id.clone()),
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    let hint = &result.output["session_hint"];
    assert_eq!(hint["has_open_messages"], true);
    assert_eq!(hint["open_counts"]["guidance"], 0);
    assert_eq!(hint["open_counts"]["question"], 1);
    assert_eq!(hint["open_counts"]["todo"], 1);
    assert_eq!(hint["open_counts"]["risk"], 1);
    assert_eq!(hint["highest_priority"], "normal");
    assert_eq!(hint["suggested_next_tool"], "session_discussion_summary");
    assert!(hint.get("attention_required").is_none());
    assert!(hint.get("attention_reason").is_none());
    assert!(hint.get("attention_instruction").is_none());
}

#[tokio::test]
async fn session_inbox_hint_disappears_after_message_resolved() {
    let client_id = "inbox-resolve";
    let runtime = runtime_with_agent_project(client_id);
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
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("resolve".to_string()));
    let message_id = post_session_message(
        &runtime,
        &session.session_id,
        SessionMessageKind::Todo,
        SessionMessagePriority::High,
        "resolve me",
    )
    .await;

    let before = read_agent_file_for_session(
        &runtime,
        client_id,
        project.clone(),
        Some(session.session_id.clone()),
    )
    .await;
    assert!(before.output.get("session_hint").is_some());

    let resolved = runtime
        .dispatch(ToolCall::ResolveSessionMessage {
            session_id: session.session_id.clone(),
            message_id,
            resolution: Some("done".to_string()),
        })
        .await;
    assert!(resolved.success, "{:?}", resolved.error);

    let after = read_agent_file_for_session(
        &runtime,
        client_id,
        project,
        Some(session.session_id.clone()),
    )
    .await;
    assert!(after.success, "{:?}", after.error);
    assert!(after.output.get("session_hint").is_none());
}

#[tokio::test]
async fn start_session_without_project_is_allowed() {
    let runtime = test_runtime();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::StartSession {
                project: None,
                title: Some("probe".to_string()),
                mode: SessionMode::Normal,
                deny_write_tools: false,
                deny_shell_tools: false,
                execution_context: None,
            },
            None,
        )
        .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["project"], Value::Null);
    assert_eq!(result.output["project_input"], Value::Null);
    assert_eq!(result.output["resolved_project"], Value::Null);
    // No project => project_instructions must be null (not loaded=false).
    assert_eq!(result.output["project_instructions"], Value::Null);
}

#[tokio::test]
async fn start_session_valid_full_id_stores_resolved_project() {
    let runtime = runtime_with_resolver_projects().await;
    let result = runtime
        .dispatch_with_auth(
            ToolCall::StartSession {
                project: Some("agent:workstation:my-repo".to_string()),
                title: Some("probe".to_string()),
                mode: SessionMode::Normal,
                deny_write_tools: false,
                deny_shell_tools: false,
                execution_context: Some(sessions::SessionExecutionContext {
                    default_cwd: Some("frontend/./src".to_string()),
                    default_shell: Some(ExecutionShell::Bash),
                    resource: None,
                }),
            },
            None,
        )
        .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["project"], "agent:workstation:my-repo");
    assert_eq!(result.output["project_input"], "agent:workstation:my-repo");
    assert_eq!(
        result.output["resolved_project"],
        "agent:workstation:my-repo"
    );
    assert_eq!(
        result.output["execution_context"],
        serde_json::json!({
            "default_cwd": "frontend/src",
            "default_shell": "bash"
        })
    );
    let summary = runtime
        .sessions
        .summary(result.output["session_id"].as_str().unwrap(), None)
        .unwrap();
    assert_eq!(
        summary.execution_context.default_cwd.as_deref(),
        Some("frontend/src")
    );
    assert_eq!(
        summary.execution_context.default_shell,
        Some(ExecutionShell::Bash)
    );
}

#[tokio::test]
async fn start_session_valid_short_id_stores_resolved_project() {
    let runtime = runtime_with_resolver_projects().await;
    let result = runtime
        .dispatch_with_auth(
            ToolCall::StartSession {
                project: Some("other-repo".to_string()),
                title: Some("probe".to_string()),
                mode: SessionMode::Normal,
                deny_write_tools: false,
                deny_shell_tools: false,
                execution_context: None,
            },
            None,
        )
        .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["project"], "agent:workstation:other-repo");
    assert_eq!(result.output["project_input"], "other-repo");
    assert_eq!(
        result.output["resolved_project"],
        "agent:workstation:other-repo"
    );
}

#[tokio::test]
async fn start_session_ambiguous_project_fails_with_candidates() {
    let runtime = runtime_with_resolver_projects().await;
    let result = runtime
        .dispatch_with_auth(
            ToolCall::StartSession {
                project: Some("my-repo".to_string()),
                title: Some("probe".to_string()),
                mode: SessionMode::Normal,
                deny_write_tools: false,
                deny_shell_tools: false,
                execution_context: None,
            },
            None,
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "ambiguous_project");
    let candidates = result.output["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0]["id"], "agent:laptop:my-repo");
    assert_eq!(candidates[1]["id"], "agent:workstation:my-repo");
}

#[tokio::test]
async fn start_session_unknown_project_fails_with_candidates() {
    let runtime = runtime_with_resolver_projects().await;
    let result = runtime
        .dispatch_with_auth(
            ToolCall::StartSession {
                project: Some("missing-repo".to_string()),
                title: Some("probe".to_string()),
                mode: SessionMode::Normal,
                deny_write_tools: false,
                deny_shell_tools: false,
                execution_context: None,
            },
            None,
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "unknown_project");
    assert_eq!(result.output["project"], "missing-repo");
    assert!(result.output["candidates"].as_array().unwrap().len() >= 3);
}

// --- Phase 2: close_session tool ---

#[tokio::test]
async fn close_session_tool_active_to_closed_and_query_still_works() {
    let runtime = test_runtime();
    let started = runtime
        .dispatch(ToolCall::StartSession {
            project: None,
            title: Some("close tool".to_string()),
            mode: SessionMode::Normal,
            deny_write_tools: false,
            deny_shell_tools: false,
            execution_context: None,
        })
        .await;
    assert!(started.success, "{:?}", started.error);
    let session_id = started.output["session_id"].as_str().unwrap().to_string();
    assert_eq!(started.output["lifecycle"], "active");

    let closed = runtime
        .dispatch(ToolCall::CloseSession {
            session_id: session_id.clone(),
        })
        .await;
    assert!(closed.success, "{:?}", closed.error);
    assert_eq!(closed.output["lifecycle"], "closed");
    assert_eq!(closed.output["already_closed"], false);
    assert_eq!(closed.output["session_id"], session_id);

    let summary = runtime
        .dispatch(ToolCall::SessionSummary {
            session_id: session_id.clone(),
            limit: Some(20),
        })
        .await;
    assert!(summary.success, "{:?}", summary.error);
    assert_eq!(summary.output["lifecycle"], "closed");
    assert_eq!(summary.output["session_id"], session_id);

    let again = runtime
        .dispatch(ToolCall::CloseSession {
            session_id: session_id.clone(),
        })
        .await;
    assert!(again.success, "{:?}", again.error);
    assert_eq!(again.output["already_closed"], true);
    assert_eq!(again.output["lifecycle"], "closed");
}

#[tokio::test]
async fn close_session_unknown_id_fails_without_create() {
    let runtime = test_runtime();
    let missing = "wc_sess_toolmissingclose01";
    let result = runtime
        .dispatch(ToolCall::CloseSession {
            session_id: missing.to_string(),
        })
        .await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "unknown_session_id");
    assert_eq!(result.output["session_id"], missing);
    assert!(!runtime.sessions.contains_session(missing));
}

#[tokio::test]
async fn close_session_requires_explicit_session_id_no_current_fallback() {
    // close_session must not accept empty/missing business session_id via parse.
    let err = ToolCall::from_tool_name("close_session", serde_json::json!({})).unwrap_err();
    assert!(
        err.contains("session_id") || err.contains("missing field"),
        "expected required session_id parse failure, got: {err}"
    );
    let err_null =
        ToolCall::from_tool_name("close_session", serde_json::json!({"session_id": null}))
            .unwrap_err();
    assert!(
        err_null.contains("session_id") || err_null.contains("invalid"),
        "expected null session_id rejection, got: {err_null}"
    );
}

#[tokio::test]
async fn closed_session_blocks_write_tools_and_message_post() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("README.md"), "hello\n").unwrap();
    let runtime = runtime_with_project(root, "demo");
    let session = runtime
        .sessions
        .start_session(Some("demo".to_string()), Some("closed writes".to_string()));
    runtime.sessions.close_session(&session.session_id).unwrap();

    let write = runtime
        .dispatch(ToolCall::WriteProjectFile {
            project: "demo".to_string(),
            path: "should-not-write.txt".to_string(),
            content: "blocked".to_string(),
            session_id: Some(session.session_id.clone()),
            overwrite: None,
            expected_sha256: None,
        })
        .await;
    assert!(!write.success);
    assert_eq!(write.output["error_kind"], "session_closed");
    assert_eq!(write.output["lifecycle"], "closed");
    assert!(!root.join("should-not-write.txt").exists());

    let post = runtime
        .dispatch(ToolCall::PostSessionMessage {
            session_id: session.session_id.clone(),
            kind: SessionMessageKind::Note,
            message: "after close".to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::Normal,
            requires_ack: false,
        })
        .await;
    assert!(!post.success);
    assert_eq!(post.output["error_kind"], "session_closed");
}

#[tokio::test]
async fn finish_coding_task_does_not_auto_close_session() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "coding-finish-no-close", "demo", tmp.path())
            .await;
    let auth = auth_context(None, true);
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("finish no close".to_string()));
    assert_eq!(
        runtime
            .sessions
            .lifecycle_state(&session.session_id)
            .unwrap(),
        super::super::sessions::SessionLifecycle::Active
    );

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only: true,
                        include_diff: Some(false),
                        include_workspace: Some(true),
                        include_hygiene: Some(false),
                        include_handoff: Some(false),
                        include_validation_summary: Some(false),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "coding-finish-no-close").await;
    let show_changes_stdout =
        crate::tool_runtime::framed_clean_show_changes_test_stdout("add readme", false);
    complete_patch_agent_request(
        &runtime,
        "coding-finish-no-close",
        &req.request_id,
        0,
        &show_changes_stdout,
        "",
    )
    .await;
    let finish = task.await.unwrap();
    assert!(finish.success, "{:?}", finish.error);
    assert_eq!(
        runtime
            .sessions
            .lifecycle_state(&session.session_id)
            .unwrap(),
        super::super::sessions::SessionLifecycle::Active,
        "finish_coding_task must remain report-only (finish != close)"
    );
}
