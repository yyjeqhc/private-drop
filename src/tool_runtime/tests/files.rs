//! Files tests for tool_runtime.

use super::super::files::*;
use super::super::helpers::*;
use super::super::*;
use super::support::*;
use crate::runner_protocol::{RunnerCapabilities, RunnerRequest, RunnerResultRequest};
use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(windows)]
async fn run_windows_tracked_listing(
    runtime: &ToolRuntime,
    client_id: &str,
    project: String,
    path: Option<String>,
) -> (ToolResult, String) {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .list_project_tracked_files(project, path, None, None, Some(100), Some(0))
                .await
        }
    });
    let request = wait_for_patch_agent_request(runtime, client_id).await;
    assert_eq!(request.kind, "run_internal_posix_script");
    assert!(request.command.is_empty());
    let payload = request
        .script
        .as_ref()
        .expect("tracked listing must carry a typed internal POSIX program");
    assert_eq!(
        payload.language,
        crate::runner_protocol::ShellScriptLanguage::Sh
    );
    let script = payload.script.clone();
    let (exit_code, stdout, stderr) = run_runner_shell_request_locally(&request);
    complete_patch_agent_request(
        runtime,
        client_id,
        &request.request_id,
        exit_code,
        &stdout,
        &stderr,
    )
    .await;
    (task.await.unwrap(), script)
}

#[cfg(windows)]
#[tokio::test]
async fn windows_list_project_tracked_files_uses_internal_posix_and_preserves_scope() {
    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    std::fs::create_dir_all(repo.path().join("src/nested")).unwrap();
    commit_file(repo.path(), "root.txt", "root\n", "root file");
    commit_file(repo.path(), "src/a.rs", "pub fn a() {}\n", "src file");
    commit_file(
        repo.path(),
        "src/nested/b.rs",
        "pub fn b() {}\n",
        "nested file",
    );

    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "tracked-windows", "demo", repo.path()).await;
    let (root, script) =
        run_windows_tracked_listing(&runtime, "tracked-windows", project.clone(), None).await;
    assert!(root.success, "{:?}", root.error);
    assert!(script.contains("git ls-files -z --cached"));
    assert!(
        script.contains("head_cmd")
            && script.contains(&format!("-c {}", LIST_TRACKED_SOURCE_MAX_BYTES + 1)),
        "tracked listing lost its raw-output cap: {script}"
    );
    let root_paths = root.output["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(root_paths, vec!["root.txt", "src/a.rs", "src/nested/b.rs"]);
    assert_eq!(root.output["source"], "git_index");
    assert_eq!(root.output["list_truncated"], false);

    let (scoped, _) = run_windows_tracked_listing(
        &runtime,
        "tracked-windows",
        project,
        Some("src".to_string()),
    )
    .await;
    assert!(scoped.success, "{:?}", scoped.error);
    assert_eq!(scoped.output["path"], "src");
    let scoped_paths = scoped.output["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(scoped_paths, vec!["src/a.rs", "src/nested/b.rs"]);
}

#[cfg(windows)]
#[tokio::test]
async fn windows_list_project_tracked_files_keeps_non_git_error_contract() {
    let project_dir = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "tracked-non-git", "demo", project_dir.path())
            .await;
    let (result, _) = run_windows_tracked_listing(&runtime, "tracked-non-git", project, None).await;
    assert!(!result.success);
    assert_eq!(result.output["code"], "not_a_git_repository");
}

#[cfg(windows)]
#[tokio::test]
async fn tracked_listing_failure_keeps_bounded_multiline_stderr() {
    let project_dir = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "tracked-diagnostic", "demo", project_dir.path())
            .await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .list_project_tracked_files(project, None, None, None, Some(100), Some(0))
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "tracked-diagnostic").await;
    assert_eq!(request.kind, "run_internal_posix_script");
    let stderr = format!(
        "EARLY_DIAGNOSTIC_MUST_BE_TRUNCATED\n{}\nACTIONABLE_SECOND_LINE\nACTIONABLE_LAST_LINE",
        "x".repeat(LIST_TRACKED_STDERR_MAX_CHARS + 1024)
    );
    complete_patch_agent_request(
        &runtime,
        "tracked-diagnostic",
        &request.request_id,
        1,
        "",
        &stderr,
    )
    .await;
    let result = task.await.unwrap();
    assert!(!result.success);
    let error = result.error.as_deref().unwrap_or_default();
    assert!(error.contains("ACTIONABLE_SECOND_LINE"), "{error}");
    assert!(error.contains("ACTIONABLE_LAST_LINE"), "{error}");
    assert!(
        error.contains('\n'),
        "stderr excerpt must remain multi-line: {error}"
    );
    assert!(!error.contains("EARLY_DIAGNOSTIC_MUST_BE_TRUNCATED"));
    assert!(
        error.chars().count() <= LIST_TRACKED_STDERR_MAX_CHARS + 64,
        "bounded stderr grew unexpectedly: {} chars",
        error.chars().count()
    );
}

async fn run_mocked_tracked_listing(
    client_id: &str,
    stdout: String,
    depth: Option<usize>,
    limit: usize,
    offset: usize,
) -> (ToolResult, String) {
    let runtime = runtime_with_agent_project(client_id);
    register_agent(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            shell: true,
            internal_posix_script: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id(client_id);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .list_project_tracked_files(project, None, None, depth, Some(limit), Some(offset))
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, client_id).await;
    assert_eq!(request.kind, "run_internal_posix_script");
    let script = request
        .script
        .as_ref()
        .expect("tracked listing must use the typed internal POSIX path")
        .script
        .clone();
    complete_patch_agent_request(&runtime, client_id, &request.request_id, 0, &stdout, "").await;
    (task.await.unwrap(), script)
}

#[tokio::test]
async fn tracked_listing_source_budget_is_below_default_retention_and_disables_fake_paging() {
    const ORDINARY_RESULT_RETENTION_COMPATIBILITY_FLOOR_BYTES: usize = 256 * 1024;
    assert!(
        LIST_TRACKED_SOURCE_MAX_BYTES + 1 < ORDINARY_RESULT_RETENTION_COMPATIBILITY_FLOOR_BYTES,
        "producer source probe must keep headroom below ordinary per-stream result retention"
    );

    let mut raw = String::new();
    let mut index = 0usize;
    while raw.len() <= LIST_TRACKED_SOURCE_MAX_BYTES + 4096 {
        raw.push_str(&format!("src/file-{index:05}-{}.rs\0", "x".repeat(20)));
        index += 1;
    }
    assert!(raw.len() < ORDINARY_RESULT_RETENTION_COMPATIBILITY_FLOOR_BYTES);

    let (result, script) =
        run_mocked_tracked_listing("tracked-source-budget", raw, Some(16), 10, 0).await;
    assert!(result.success, "{:?}", result.error);
    assert!(script.contains(&format!("-c {}", LIST_TRACKED_SOURCE_MAX_BYTES + 1)));
    assert_eq!(result.output["list_truncated"], true);
    assert_eq!(result.output["truncated"], false);
    assert_eq!(result.output["next_offset"], Value::Null);
    assert_eq!(result.output["returned"], 10);
    assert!(result.output["total_files"].as_u64().unwrap() >= 10);
}

#[tokio::test]
async fn tracked_listing_fails_closed_when_server_retains_only_stdout_tail() {
    let mut raw = String::new();
    let mut index = 0usize;
    while raw.len() <= 300 * 1024 {
        raw.push_str(&format!("src/file-{index:05}-{}.rs\0", "y".repeat(32)));
        index += 1;
    }

    let (result, _) =
        run_mocked_tracked_listing("tracked-retained-tail", raw, Some(16), 10, 0).await;
    assert!(!result.success);
    assert_eq!(result.output["code"], "source_incomplete");
    assert!(result.output.get("next_offset").is_none());
    assert!(result.output.get("total_files").is_none());
    assert!(result
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("ordinary result retention"));
}

#[tokio::test]
async fn write_project_file_with_session_id_records_changed_path_without_content() {
    let runtime = runtime_with_agent_project("telemetry-write");
    let caps = RunnerCapabilities {
        file_write: true,
        shell: true,
        git: true,
        internal_posix_script: true,
        ..Default::default()
    };
    register_agent(&runtime, "telemetry-write", None, caps).await;
    let project = agent_test_project_id("telemetry-write");
    let session = runtime.sessions.start_session(Some(project.clone()), None);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::WriteProjectFile {
                        project,
                        path: "src/new.txt".to_string(),
                        content: "do-not-log-this-content\n".to_string(),
                        session_id: Some(session_id),
                        overwrite: None,
                        expected_sha256: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "telemetry-write").await;
    assert_eq!(req.kind, "file_write_project_file");
    assert!(req.command.is_empty());
    assert!(req.stdin.is_none());
    let payload: serde_json::Value =
        serde_json::from_str(req.content.as_deref().expect("file-op payload")).unwrap();
    assert_eq!(payload["path"], "src/new.txt");
    assert_eq!(payload["content"], "do-not-log-this-content\n");
    complete_patch_agent_request(
        &runtime,
        "telemetry-write",
        &req.request_id,
        0,
        r#"{"path":"src/new.txt","bytes_written":24,"sha256":"abc","changed":true,"state_changed":true,"execution_state":"completed"}"#,
        "",
    )
    .await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["permission"]["required"], true);
    assert_eq!(result.output["state_changed"], true);
    assert_eq!(result.output["permission"]["policy"], "trusted_agent");
    assert_eq!(result.output["permission"]["status"], "auto_approved");
    assert_eq!(
        result.output["permission"]["reason"],
        "trusted_agent_authority"
    );
    assert_eq!(result.output["permission"]["risk"], "write");
    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    assert_eq!(summary.counts.write_like, 1);
    let event = finished_event(&summary, "write_project_file");
    assert!(event.write_like);
    assert_eq!(event.changed_paths, vec!["src/new.txt".to_string()]);
    assert_eq!(
        event
            .effect_evidence
            .as_ref()
            .and_then(|evidence| evidence.state_changed),
        Some(true)
    );
    let permission = event.permission.as_ref().expect("permission metadata");
    assert!(permission.required);
    assert_eq!(permission.policy, "trusted_agent");
    assert_eq!(permission.status, "auto_approved");
    assert_eq!(permission.tool_name, "write_project_file");
    assert_eq!(permission.risk, "write");
    let serialized = serde_json::to_string(&summary.events).unwrap();
    assert!(
        !serialized.contains("do-not-log-this-content"),
        "session event leaked write content: {serialized}"
    );

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
    assert_eq!(handoff.output["permissions"]["required_count"], 1);
    assert_eq!(handoff.output["permissions"]["auto_approved_count"], 1);
    assert_eq!(handoff.output["permissions"]["manual_approved_count"], 0);
    assert_eq!(handoff.output["permissions"]["total_approved_count"], 1);
    assert_eq!(
        handoff.output["permissions"]["recent"][0]["tool_name"],
        "write_project_file"
    );

    let finish_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only: false,
                        include_diff: Some(false),
                        include_workspace: None,
                        include_hygiene: Some(false),
                        include_handoff: Some(false),
                        include_validation_summary: Some(false),
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "telemetry-write").await;
    assert_internal_posix_script_contains(&req, "git status --porcelain=v1 -b");
    let show_changes_stdout =
        crate::tool_runtime::framed_clean_show_changes_test_stdout("write", false);
    complete_patch_agent_request(
        &runtime,
        "telemetry-write",
        &req.request_id,
        0,
        &show_changes_stdout,
        "",
    )
    .await;
    let finish = finish_task.await.unwrap();
    assert!(finish.success, "{:?}", finish.error);
    assert_eq!(finish.output["permissions"]["required_count"], 1);
    assert_eq!(finish.output["permissions"]["auto_approved_count"], 1);
    assert_eq!(finish.output["permissions"]["manual_approved_count"], 0);
    assert_eq!(finish.output["permissions"]["total_approved_count"], 1);
}

#[tokio::test]
async fn delete_project_files_capable_agent_uses_structured_delete_without_output_leaks() {
    let runtime = runtime_with_agent_project("cleanup-delete");
    register_agent(
        &runtime,
        "cleanup-delete",
        None,
        RunnerCapabilities {
            structured_file_delete: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("cleanup-delete");
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .delete_project_files(project, vec!["tmp.txt".to_string()])
                .await
        }
    });

    let req = wait_for_patch_agent_request(&runtime, "cleanup-delete").await;
    assert_eq!(req.kind, "file_delete_project_files");
    assert!(req.command.is_empty());
    assert_eq!(req.path.as_deref(), Some("."));
    let payload: Value = serde_json::from_str(req.content.as_deref().unwrap()).unwrap();
    assert_eq!(payload, json!({"paths": ["tmp.txt"]}));
    complete_patch_agent_request(
        &runtime,
        "cleanup-delete",
        &req.request_id,
        0,
        r#"{"deleted_paths":["tmp.txt"]}"#,
        "/private/runner/path raw stderr must not leak\n",
    )
    .await;

    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["ok"], true);
    assert_eq!(result.output["state_changed"], true);
    assert_eq!(result.output["deleted_paths"], json!(["tmp.txt"]));
    assert_eq!(result.output["stdout_present"], false);
    assert_eq!(result.output["stderr_present"], false);
    let serialized = serde_json::to_string(&result.output).unwrap();
    assert!(!serialized.contains("/private/runner/path"));
    assert!(!serialized.contains("raw stderr"));
}

/// Wait until the client has at least `expected` pending requests without
/// polling/dispatching them. The single wall-clock deadline is deliberately
/// independent of scheduler yield counts.
async fn wait_for_pending_requests(runtime: &ToolRuntime, client_id: &str, expected: usize) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let view = runtime
            .runner_registry
            .get_runner_view(client_id)
            .await
            .unwrap_or_else(|| panic!("client {client_id} must be registered"));
        let pending = view.pending_requests;
        if pending >= expected {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "pending requests did not reach {expected} within 10 seconds for {client_id}; last pending count={pending}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn delete_project_files_replacement_before_poll_reports_not_started() {
    let runtime = runtime_with_agent_project("cleanup-delete-replace-early");
    register_agent(
        &runtime,
        "cleanup-delete-replace-early",
        None,
        RunnerCapabilities {
            structured_file_delete: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("cleanup-delete-replace-early");
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .delete_project_files(project, vec!["tmp.txt".to_string()])
                .await
        }
    });

    // Wait until the structured request is queued, without polling it.
    wait_for_pending_requests(&runtime, "cleanup-delete-replace-early", 1).await;
    // Replace the Runner process before the request was ever polled.
    runtime
        .runner_registry
        .set_last_seen_for_test(
            "cleanup-delete-replace-early",
            chrono::Utc::now().timestamp() - 120,
        )
        .await;
    register_agent_with_instance(
        &runtime,
        "cleanup-delete-replace-early",
        "inst-b",
        None,
        RunnerCapabilities::default(),
    )
    .await;

    let result = task.await.unwrap();
    assert!(!result.success, "{:?}", result.error);
    assert_eq!(result.output["execution_state"], "not_started");
    assert_eq!(result.output["failure_kind"], "not_started");
    assert_eq!(result.output["tool_failure"], true);
    assert_ne!(result.output["execution_state"], "outcome_unknown");
    let error = result.error.as_deref().unwrap_or_default();
    assert!(
        error.contains("was not dispatched") && error.contains("did not start"),
        "error was: {error}"
    );

    // No legacy fallback and no inherited structured request for the
    // replacement Runner.
    let extra =
        probe_agent_request_for_instance(&runtime, "cleanup-delete-replace-early", "inst-b").await;
    assert!(
        extra.is_none(),
        "replacement Runner must receive no request: {extra:?}"
    );
}

#[tokio::test]
async fn delete_project_files_replacement_after_poll_reports_outcome_unknown() {
    let runtime = runtime_with_agent_project("cleanup-delete-replace-late");
    register_agent(
        &runtime,
        "cleanup-delete-replace-late",
        None,
        RunnerCapabilities {
            structured_file_delete: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("cleanup-delete-replace-late");
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .delete_project_files(project, vec!["tmp.txt".to_string()])
                .await
        }
    });

    wait_for_pending_requests(&runtime, "cleanup-delete-replace-late", 1).await;
    // Dispatch the structured request to the original instance.
    let req =
        wait_for_runner_request_for_instance(&runtime, "cleanup-delete-replace-late", "inst").await;
    assert_eq!(req.kind, "file_delete_project_files");
    // Replace the Runner before it returns its result.
    runtime
        .runner_registry
        .set_last_seen_for_test(
            "cleanup-delete-replace-late",
            chrono::Utc::now().timestamp() - 120,
        )
        .await;
    register_agent_with_instance(
        &runtime,
        "cleanup-delete-replace-late",
        "inst-b",
        None,
        RunnerCapabilities::default(),
    )
    .await;

    let result = task.await.unwrap();
    assert!(!result.success, "{:?}", result.error);
    assert_eq!(result.output["execution_state"], "outcome_unknown");
    assert_eq!(result.output["failure_kind"], "outcome_unknown");
    assert_eq!(result.output["tool_failure"], true);
    let error = result.error.as_deref().unwrap_or_default();
    assert!(
        error.contains("may already have deleted files"),
        "error was: {error}"
    );
    assert!(
        error.contains("Inspect current workspace state"),
        "error was: {error}"
    );

    // The replacement Runner cannot complete or inherit the dispatched
    // request, and no legacy fallback is emitted.
    let err = runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: "cleanup-delete-replace-late".to_string(),
            runner_instance_id: "inst-b".to_string(),
            request_id: req.request_id,
            exit_code: Some(0),
            stdout: Some(r#"{"deleted_paths":["tmp.txt"]}"#.to_string()),
            stderr: None,
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap_err();
    assert!(
        err.contains("unknown or expired shell request"),
        "replacement must not complete the replaced request: {err}"
    );
    let extra =
        probe_agent_request_for_instance(&runtime, "cleanup-delete-replace-late", "inst-b").await;
    assert!(
        extra.is_none(),
        "replacement Runner must receive no inherited request: {extra:?}"
    );
}

#[tokio::test]
async fn delete_project_files_timeout_before_dispatch_reports_not_started() {
    let runtime = runtime_with_agent_project("cleanup-delete-timeout-early");
    register_agent(
        &runtime,
        "cleanup-delete-timeout-early",
        None,
        RunnerCapabilities {
            structured_file_delete: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("cleanup-delete-timeout-early");
    let proj = runtime.resolve_project(&project).await.unwrap();
    let client_id = proj.client_id.clone();

    // Short wait bound: the request is never polled, so the wait timeout fires
    // and the dispatch-aware cancellation proves the request was never
    // dispatched.
    let result = runtime
        .delete_project_files_structured_agent(&proj, client_id, vec!["tmp.txt".to_string()], 1)
        .await;

    assert!(!result.success, "{:?}", result.error);
    assert_eq!(result.output["execution_state"], "not_started");
    assert_eq!(result.output["failure_kind"], "not_started");
    assert_eq!(result.output["tool_failure"], true);
    let error = result.error.as_deref().unwrap_or_default();
    assert!(
        error.contains("timed out") && error.contains("did not start"),
        "error was: {error}"
    );
    // The timed-out request was removed: no queue/waiter leak.
    let view = runtime
        .runner_registry
        .get_runner_view("cleanup-delete-timeout-early")
        .await
        .unwrap();
    assert_eq!(view.pending_requests, 0);
}

#[tokio::test]
async fn delete_project_files_timeout_after_dispatch_reports_outcome_unknown() {
    let runtime = runtime_with_agent_project("cleanup-delete-timeout-late");
    register_agent(
        &runtime,
        "cleanup-delete-timeout-late",
        None,
        RunnerCapabilities {
            structured_file_delete: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("cleanup-delete-timeout-late");
    let proj = runtime.resolve_project(&project).await.unwrap();
    let client_id = proj.client_id.clone();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let client_id = client_id.clone();
        async move {
            runtime
                .delete_project_files_structured_agent(
                    &proj,
                    client_id,
                    vec!["tmp.txt".to_string()],
                    1,
                )
                .await
        }
    });

    wait_for_pending_requests(&runtime, "cleanup-delete-timeout-late", 1).await;
    // Dispatch the structured request; the Runner never returns a result, so
    // the wait timeout fires after dispatch may have started deleting.
    let req =
        wait_for_runner_request_for_instance(&runtime, "cleanup-delete-timeout-late", "inst").await;
    assert_eq!(req.kind, "file_delete_project_files");

    let result = task.await.unwrap();
    assert!(!result.success, "{:?}", result.error);
    assert_eq!(result.output["execution_state"], "outcome_unknown");
    assert_eq!(result.output["failure_kind"], "outcome_unknown");
    let error = result.error.as_deref().unwrap_or_default();
    assert!(
        error.contains("timed out") && error.contains("may already have deleted files"),
        "error was: {error}"
    );
    // The timed-out request was removed: no queue/waiter leak.
    let view = runtime
        .runner_registry
        .get_runner_view("cleanup-delete-timeout-late")
        .await
        .unwrap();
    assert_eq!(view.pending_requests, 0);
}

#[tokio::test]
async fn delete_project_files_waiter_dropped_without_undispatch_proof_reports_outcome_unknown() {
    let runtime = runtime_with_agent_project("cleanup-delete-waiter");
    register_agent(
        &runtime,
        "cleanup-delete-waiter",
        None,
        RunnerCapabilities {
            structured_file_delete: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("cleanup-delete-waiter");
    let proj = runtime.resolve_project(&project).await.unwrap();
    let client_id = proj.client_id.clone();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let client_id = client_id.clone();
        async move {
            runtime
                .delete_project_files_structured_agent(
                    &proj,
                    client_id,
                    vec!["tmp.txt".to_string()],
                    30,
                )
                .await
        }
    });

    wait_for_pending_requests(&runtime, "cleanup-delete-waiter", 1).await;
    // Manufacture the dropped waiter through the existing dispatch-state-aware
    // cancellation API: remove the pending record (dropping the oneshot
    // sender) without resolving it, so the tool's receiver observes the
    // channel close. The registry returns the preserved dispatch truth.
    let req = wait_for_runner_request_for_instance(&runtime, "cleanup-delete-waiter", "inst").await;
    let dispatch = runtime
        .runner_registry
        .cancel_request_dispatch_state(&req.request_id)
        .await;
    assert_eq!(
        dispatch,
        Some(true),
        "registry must preserve dispatch truth for the cancelled request"
    );

    // The subsequent cancellation in the tool finds no record, which cannot
    // prove undispatch: the result must be outcome_unknown, never not_started.
    let result = task.await.unwrap();
    assert!(!result.success, "{:?}", result.error);
    assert_eq!(result.output["execution_state"], "outcome_unknown");
    assert_eq!(result.output["failure_kind"], "outcome_unknown");
    assert_eq!(result.output["tool_failure"], true);
    let error = result.error.as_deref().unwrap_or_default();
    assert!(
        error.contains("may already have deleted files"),
        "error was: {error}"
    );
    let view = runtime
        .runner_registry
        .get_runner_view("cleanup-delete-waiter")
        .await
        .unwrap();
    assert_eq!(view.pending_requests, 0);
}

#[tokio::test]
async fn delete_project_files_terminal_failure_reports_outcome_unknown() {
    let runtime = runtime_with_agent_project("cleanup-delete-terminal");
    register_agent(
        &runtime,
        "cleanup-delete-terminal",
        None,
        RunnerCapabilities {
            structured_file_delete: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("cleanup-delete-terminal");
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .delete_project_files(project, vec!["tmp.txt".to_string()])
                .await
        }
    });

    wait_for_pending_requests(&runtime, "cleanup-delete-terminal", 1).await;
    // The Runner returns a definitive terminal failure after dispatch
    // (non-zero exit). The mutation may already have deleted files, so the
    // failure must never collapse into an ordinary retry-safe error.
    let req =
        wait_for_runner_request_for_instance(&runtime, "cleanup-delete-terminal", "inst").await;
    complete_patch_agent_request_for_instance(
        &runtime,
        "cleanup-delete-terminal",
        "inst",
        &req.request_id,
        1,
        "",
        "delete failed",
    )
    .await;

    let result = task.await.unwrap();
    assert!(!result.success, "{:?}", result.error);
    assert_eq!(result.output["execution_state"], "outcome_unknown");
    assert_eq!(result.output["failure_kind"], "outcome_unknown");
    assert_eq!(result.output["tool_failure"], true);
    let error = result.error.as_deref().unwrap_or_default();
    assert!(
        error.contains("may already have deleted files"),
        "error was: {error}"
    );
    // No automatic legacy fallback follows the uncertain mutation.
    let extra = probe_agent_request_for_instance(&runtime, "cleanup-delete-terminal", "inst").await;
    assert!(
        extra.is_none(),
        "no legacy fallback may follow an uncertain structured delete: {extra:?}"
    );
}

#[tokio::test]
async fn artifact_upload_chunk_session_log_arguments_do_not_store_base64() {
    let runtime = runtime_with_agent_project("telemetry-artifact-chunk");
    register_agent(
        &runtime,
        "telemetry-artifact-chunk",
        None,
        RunnerCapabilities {
            file_write: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("telemetry-artifact-chunk");
    let session = runtime.sessions.start_session(Some(project.clone()), None);
    let raw_marker = "SECRET_CHUNK_CONTENT_SHOULD_NOT_BE_LOGGED";
    let content_base64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw_marker);

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        let content_base64 = content_base64.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::ArtifactUploadChunk {
                        project,
                        path: "artifacts/imports/chunk.txt".to_string(),
                        upload_id: "wc_upload_test_1".to_string(),
                        offset: 7,
                        content_base64,
                        session_id: Some(session_id),
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });

    let req = wait_for_patch_agent_request(&runtime, "telemetry-artifact-chunk").await;
    let payload: serde_json::Value =
        serde_json::from_str(req.content.as_deref().expect("file-op payload")).unwrap();
    assert_eq!(payload["content_base64"], content_base64);
    complete_patch_agent_request(
        &runtime,
        "telemetry-artifact-chunk",
        &req.request_id,
        0,
        r#"{"path":"artifacts/imports/chunk.txt","upload_id":"wc_upload_test_1","received_bytes":12,"next_offset":12,"expected_bytes":null,"expected_sha256":null,"max_bytes":268435456,"mime_type":null,"committed":false}"#,
        "",
    )
    .await;
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);

    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    let started = summary
        .events
        .iter()
        .rev()
        .find(|event| {
            event.kind == "tool_call_started" && event.tool_name == "artifact_upload_chunk"
        })
        .expect("started event for artifact_upload_chunk");
    let input_summary = started
        .input_summary
        .as_ref()
        .expect("input_summary present on started event");
    assert_eq!(input_summary["path"], "artifacts/imports/chunk.txt");
    assert_eq!(input_summary["upload_id"], "wc_upload_test_1");
    assert_eq!(input_summary["offset"], 7);
    assert_eq!(input_summary["content_base64_present"], true);
    assert!(input_summary.get("content_base64").is_none());
    let serialized = serde_json::to_string(&summary.events).unwrap();
    assert!(
        !serialized.contains(&content_base64) && !serialized.contains(raw_marker),
        "session event leaked base64 chunk content: {serialized}"
    );
}

#[test]
fn conversation_import_session_log_arguments_do_not_store_host_file_refs() {
    let download_url = "https://files.oaiusercontent.com/NEVER_PERSIST_IMPORT_URL";
    let file_id = "NEVER_PERSIST_IMPORT_FILE_ID";
    let arguments = serde_json::json!({
        "project": "agent:test:demo",
        "openaiFileIdRefs": [{
            "download_url": download_url,
            "file_id": file_id,
            "mime_type": "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "file_name": "private-name.pptx"
        }],
        "output_dir": "paper/export",
        "targets": ["import-test.pptx"],
        "overwrite": false
    });

    let raw_summary = super::super::tool_audit::session_log_arguments_for_tool_request(
        "import_conversation_files_to_project",
        &arguments,
    );
    assert_eq!(raw_summary["project"], "agent:test:demo");
    assert_eq!(raw_summary["file_count"], 1);
    assert_eq!(raw_summary["targets_count"], 1);
    let raw_json = serde_json::to_string(&raw_summary).unwrap();
    assert!(!raw_json.contains(download_url));
    assert!(!raw_json.contains(file_id));
    assert!(!raw_json.contains("private-name.pptx"));

    let call = ToolCall::from_tool_name("import_conversation_files_to_project", arguments).unwrap();
    let typed_summary = call.session_log_arguments();
    assert_eq!(typed_summary["project"], "agent:test:demo");
    assert_eq!(typed_summary["file_count"], 1);
    assert_eq!(typed_summary["targets_count"], 1);
    let typed_json = serde_json::to_string(&typed_summary).unwrap();
    assert!(!typed_json.contains(download_url));
    assert!(!typed_json.contains(file_id));
    assert!(!typed_json.contains("private-name.pptx"));
}

#[test]
fn apply_patch_audit_records_matching_mode_without_patch_body() {
    let private_patch =
        "*** Begin Patch\n*** Add File: NEVER_LOG_PATCH_BODY.txt\n+secret\n*** End Patch";
    let arguments = serde_json::json!({
        "project": "agent:test:demo",
        "patch": private_patch,
        "dry_run": true,
        "matching_mode": "exact_unique",
    });

    let raw_summary =
        super::super::tool_audit::session_log_arguments_for_tool_request("apply_patch", &arguments);
    assert_eq!(raw_summary["project"], "agent:test:demo");
    assert_eq!(raw_summary["patch_present"], true);
    assert_eq!(raw_summary["dry_run"], true);
    assert_eq!(raw_summary["matching_mode"], "exact_unique");
    assert!(!serde_json::to_string(&raw_summary)
        .unwrap()
        .contains("NEVER_LOG_PATCH_BODY"));

    let call = ToolCall::from_tool_name("apply_patch", arguments).unwrap();
    let typed_summary = call.session_log_arguments();
    assert_eq!(typed_summary["project"], "agent:test:demo");
    assert_eq!(typed_summary["patch_present"], true);
    assert_eq!(typed_summary["dry_run"], true);
    assert_eq!(typed_summary["matching_mode"], "exact_unique");
    assert!(!serde_json::to_string(&typed_summary)
        .unwrap()
        .contains("NEVER_LOG_PATCH_BODY"));
}

#[tokio::test]
async fn conversation_import_durable_session_events_do_not_store_host_file_refs() {
    use crate::tool_runtime::kernel::{
        HostFileImportTrust, ToolCallContext, ToolCallRequest, ToolTransport,
    };

    let runtime = runtime_with_agent_project("telemetry-conversation-import");
    register_agent(
        &runtime,
        "telemetry-conversation-import",
        None,
        RunnerCapabilities {
            file_write: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("telemetry-conversation-import");
    let session = runtime.sessions.start_session(Some(project.clone()), None);
    let download_url = "https://download.example/NEVER_PERSIST_DURABLE_IMPORT_URL";
    let file_id = "NEVER_PERSIST_DURABLE_IMPORT_FILE_ID";
    let auth = auth_context(None, true);
    let outcome = runtime
        .call_tool_with_context(
            ToolCallRequest {
                tool_name: "import_conversation_files_to_project".to_string(),
                arguments: serde_json::json!({
                    "project": project,
                    "openaiFileIdRefs": [{
                        "download_url": download_url,
                        "file_id": file_id,
                        "mime_type": "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                        "file_name": "private-durable-name.pptx"
                    }],
                    "targets": ["import-test.pptx"]
                }),
            },
            ToolCallContext {
                transport: ToolTransport::Mcp,
                session_id: Some(&session.session_id),
                auth: Some(&auth),
                window: None,
                record_oauth_scope_denials: false,
                host_file_import_trust: HostFileImportTrust::Untrusted,
            },
        )
        .await;
    let result = outcome.result.expect("tool result");
    assert!(!result.success);
    assert!(result
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("explicitly trusted OAuth MCP client"));

    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    let serialized = serde_json::to_string(&summary.events).unwrap();
    assert!(!serialized.contains(download_url));
    assert!(!serialized.contains(file_id));
    assert!(!serialized.contains("private-durable-name.pptx"));
}

#[tokio::test]
async fn read_project_artifact_metadata_allow_missing_does_not_count_as_failed() {
    let runtime = runtime_with_agent_project("artifact-missing-session");
    register_agent(
        &runtime,
        "artifact-missing-session",
        None,
        RunnerCapabilities {
            file_read: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("artifact-missing-session");
    let session = runtime.sessions.start_session(Some(project.clone()), None);

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadProjectArtifactMetadata {
                        project,
                        path: "artifacts/smoke/missing.artifact".to_string(),
                        session_id: Some(session_id),
                        allow_missing: Some(true),
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });

    let req = wait_for_patch_agent_request(&runtime, "artifact-missing-session").await;
    complete_patch_agent_request(
        &runtime,
        "artifact-missing-session",
        &req.request_id,
        0,
        r#"{"path":"artifacts/smoke/missing.artifact","exists":false,"missing":true}"#,
        "",
    )
    .await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["exists"], false);
    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    assert_eq!(summary.counts.failed, 0);
    let event = finished_event(&summary, "read_project_artifact_metadata");
    assert_eq!(event.status.as_deref(), Some("succeeded"));
}

#[tokio::test]
async fn artifact_upload_begin_policy_rejection_is_classified() {
    let runtime = runtime_with_agent_project("artifact-policy-session");
    register_agent(
        &runtime,
        "artifact-policy-session",
        None,
        RunnerCapabilities {
            file_write: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("artifact-policy-session");
    let session = runtime.sessions.start_session(Some(project.clone()), None);
    let bootstrap = auth_context(None, true);

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
    assert!(result.output.get("permission").is_none());
    assert_eq!(result.output["failure_kind"], "policy_rejected");
    assert_eq!(result.output["error_kind"], "policy_rejected");
    let error = result.error.as_deref().unwrap();
    assert!(error.contains(".artifact"), "{error}");
    assert!(error.contains("artifacts/smoke/<name>.artifact"), "{error}");
    assert!(
        probe_patch_agent_request(&runtime, "artifact-policy-session")
            .await
            .is_none(),
        "policy rejection must happen before enqueue"
    );

    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    assert_eq!(summary.counts.failed, 1);
    let event = finished_event(&summary, "artifact_upload_begin");
    assert_eq!(event.failure_kind.as_deref(), Some("policy_rejected"));
    assert_eq!(event.error_kind.as_deref(), Some("policy_rejected"));
    assert!(event.permission.is_none());
}

async fn run_list_project_files_page(
    client_id: &str,
    stdout: &str,
    limit: usize,
    offset: usize,
) -> ToolResult {
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
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .list_project_files(project, None, Some(limit), Some(offset))
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, client_id).await;
    assert_eq!(request.kind, "file_list");
    assert_eq!(request.path.as_deref(), Some("."));
    complete_patch_agent_request(&runtime, client_id, &request.request_id, 0, stdout, "").await;
    task.await.unwrap()
}

#[tokio::test]
async fn list_project_files_pages_complete_source_without_gap_or_duplicate() {
    let stdout = "zeta.txt\nsrc/\nREADME.md\nCargo.toml\n.alpha\n";

    let first = run_list_project_files_page("list-files-pages", stdout, 2, 0).await;
    assert!(first.success, "{:?}", first.error);
    assert_eq!(first.output["returned"], 2);
    assert_eq!(first.output["total_entries"], 5);
    assert_eq!(first.output["offset"], 0);
    assert_eq!(first.output["next_offset"], 2);
    assert_eq!(first.output["truncated"], true);

    let second = run_list_project_files_page(
        "list-files-pages",
        stdout,
        2,
        first.output["next_offset"].as_u64().unwrap() as usize,
    )
    .await;
    assert_eq!(second.output["returned"], 2);
    assert_eq!(second.output["next_offset"], 4);

    let final_page = run_list_project_files_page(
        "list-files-pages",
        stdout,
        2,
        second.output["next_offset"].as_u64().unwrap() as usize,
    )
    .await;
    assert_eq!(final_page.output["returned"], 1);
    assert_eq!(final_page.output["next_offset"], Value::Null);
    assert_eq!(final_page.output["truncated"], false);

    let reconstructed = first.output["entries"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second.output["entries"].as_array().unwrap())
        .chain(final_page.output["entries"].as_array().unwrap())
        .map(|entry| entry["path"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        reconstructed,
        vec![".alpha", "Cargo.toml", "README.md", "src", "zeta.txt"]
    );

    let past_end = run_list_project_files_page("list-files-pages", stdout, 2, 99).await;
    assert!(past_end.success);
    assert_eq!(past_end.output["returned"], 0);
    assert_eq!(past_end.output["total_entries"], 5);
    assert_eq!(past_end.output["offset"], 99);
    assert_eq!(past_end.output["next_offset"], Value::Null);
    assert_eq!(past_end.output["truncated"], false);
}

#[tokio::test]
async fn list_project_files_fails_closed_when_runner_retained_source_is_incomplete() {
    let stdout = "[output truncated to last 262144 bytes]\nzeta.txt\nsrc/\n";
    let result = run_list_project_files_page("list-files-retained-tail", stdout, 200, 0).await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "source_incomplete");
    assert_eq!(
        result.output["reason_code"],
        "runner_result_retention_truncated"
    );
    assert!(result.output.get("next_offset").is_none());
    assert!(result.output.get("total_entries").is_none());
}

#[test]
fn parse_and_page_file_list_entries_is_sorted_gap_free_and_unicode_safe() {
    let long_name = format!("long-{}-终.rs", "x".repeat(180));
    let stdout = format!(
        "zeta.txt\nsrc/\nREADME [draft].md\n{}\n.alpha\n目录/\n",
        long_name
    );
    let all = parse_file_list_entries(&stdout, ".");
    assert_eq!(all.len(), 6);
    assert_eq!(
        all.iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            ".alpha",
            "README [draft].md",
            long_name.as_str(),
            "src",
            "zeta.txt",
            "目录",
        ]
    );
    assert_eq!(all[3]["kind"], "dir");
    assert_eq!(all[5]["kind"], "dir");

    let (first, next) = page_file_list_entries(&all, 0, 2);
    assert_eq!(next, Some(2));
    let (middle, next) = page_file_list_entries(&all, next.unwrap(), 2);
    assert_eq!(next, Some(4));
    let (final_page, next) = page_file_list_entries(&all, next.unwrap(), 2);
    assert_eq!(next, None);
    let reconstructed = first
        .into_iter()
        .chain(middle)
        .chain(final_page)
        .map(|entry| entry["path"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        reconstructed,
        all.iter()
            .map(|entry| entry["path"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    );
    let (past_end, next) = page_file_list_entries(&all, 99, 2);
    assert!(past_end.is_empty());
    assert_eq!(next, None);
}

#[test]
fn parse_file_list_entries_prepends_subpath_for_relative_paths() {
    let stdout = "main.rs\nlib.rs\n";
    let entries = parse_file_list_entries(stdout, "src");
    let paths: Vec<&str> = entries
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["src/lib.rs", "src/main.rs"]);
}

#[test]
fn validate_project_relative_path_rejects_absolute_and_parent_traversal() {
    assert!(validate_project_relative_path(".").is_ok());
    assert!(validate_project_relative_path("src").is_ok());
    assert!(validate_project_relative_path("src/main.rs").is_ok());
    assert!(validate_project_relative_path("/etc").is_err());
    assert!(validate_project_relative_path("../outside").is_err());
    assert!(validate_project_relative_path("src/../../outside").is_err());
    assert!(validate_project_relative_path("src\0main.rs").is_err());
}

#[test]
fn parse_search_matches_is_bounded_and_strips_dot_slash() {
    let stdout = "{\"webcodex_search\":{\"backend\":\"rg\"}}\n./src/main.rs:10:fn main() {}\n./src/lib.rs:3:pub fn x()\n./src/a:1:1\n";
    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(2),
        ..raw_search_request()
    })
    .unwrap();
    let result = search_project_text_output("demo", &options, stdout, Some(0), "");
    let matches = result.output["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 2);
    assert_eq!(result.output["truncated"], true);
    assert_eq!(matches[0]["path"], "src/main.rs");
    assert_eq!(matches[0]["line"], 10);
    assert_eq!(matches[0]["preview"], "fn main() {}");
    assert_eq!(matches[0]["context_before"], json!([]));
    assert_eq!(matches[0]["context_after"], json!([]));
    assert_eq!(
        matches[0]["read_hint"],
        json!({"path": "src/main.rs", "start_line": 1, "limit": 80})
    );
    assert_eq!(matches[1]["path"], "src/lib.rs");
    assert_eq!(
        matches[1]["read_hint"],
        json!({"path": "src/lib.rs", "start_line": 1, "limit": 80})
    );
}

#[test]
fn search_match_read_hint_is_deterministic_and_centers_later_matches() {
    let stdout =
        "{\"webcodex_search\":{\"backend\":\"rg\"}}\nsrc/lib.rs:42:needle\nsrc/lib.rs:120:later\n";
    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(10),
        ..raw_search_request()
    })
    .unwrap();
    let result = search_project_text_output("demo", &options, stdout, Some(0), "");
    let matches = result.output["matches"].as_array().unwrap();
    assert_eq!(
        matches[0]["read_hint"],
        json!({"path": "src/lib.rs", "start_line": 22, "limit": 80})
    );
    assert_eq!(
        matches[1]["read_hint"],
        json!({"path": "src/lib.rs", "start_line": 100, "limit": 80})
    );
}

#[test]
fn parse_search_matches_skips_lines_without_line_number() {
    // Binary file matches or malformed lines are skipped, not counted.
    let stdout = "{\"webcodex_search\":{\"backend\":\"rg\"}}\nbinary:file\nsrc/main.rs:5:hit\n";
    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(10),
        ..raw_search_request()
    })
    .unwrap();
    let result = search_project_text_output("demo", &options, stdout, Some(0), "");
    let matches = result.output["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["path"], "src/main.rs");
}

#[test]
fn parse_search_matches_drops_claude_worktree_records() {
    let stdout = concat!(
        "{\"webcodex_search\":{\"backend\":\"native\"}}\n",
        ".claude/worktrees/stale/src/lib.rs:1:needle stale\n",
        "src/lib.rs:1:needle active\n",
    );
    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(10),
        ..raw_search_request()
    })
    .unwrap();
    let result = search_project_text_output("demo", &options, stdout, Some(0), "");
    let matches = result.output["matches"].as_array().unwrap();

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["path"], "src/lib.rs");
    assert_eq!(matches[0]["preview"], "needle active");
}

#[test]
fn search_project_text_command_excludes_sensitive_dirs_and_bounds_output() {
    let options = SearchOptions::normalize(SearchRequest {
        pattern: "fn main".to_string(),
        path: Some("src".to_string()),
        limit: Some(25),
        context_before: None,
        context_after: None,
        include_globs: None,
        exclude_globs: None,
        result_mode: None,
        timeout_secs: None,
    })
    .unwrap();
    let cmd = search_project_text_command(&options);
    assert!(cmd.contains("command -v rg"));
    assert!(cmd.contains("\"backend\":\"rg\""));
    assert!(cmd.contains("\"backend\":\"grep\""));
    assert!(cmd.contains("rg --with-filename"));
    assert!(cmd.contains("--glob '!**/.git/**'"));
    assert!(cmd.contains("--glob '!**/.claude/**'"));
    assert!(cmd.contains("--glob '!**/target/**'"));
    assert!(cmd.contains("--glob '!**/node_modules/**'"));
    assert!(cmd.contains("--exclude-dir=.git"));
    assert!(cmd.contains("--exclude-dir=.claude"));
    assert!(cmd.contains("--exclude-dir=target"));
    assert!(cmd.contains("--exclude-dir=node_modules"));
    assert!(cmd.contains("--exclude-dir=secrets"));
    assert!(cmd.contains("--exclude=.env"));
    assert!(cmd.contains("--exclude=*.key"));
    assert!(cmd.contains("\"$head_cmd\" -n 26") || cmd.contains("$head_cmd -n 26"));
    assert!(cmd.contains("trap 'cleanup_search_status' EXIT"));
    assert!(cmd.contains("trap 'cleanup_search_status; exit 143' HUP INT TERM"));
    assert!(cmd.contains("grep -rnI"));
    assert!(cmd.contains("command -v head"));
    assert!(cmd.contains("/usr/bin/head") || cmd.contains("/bin/head"));
    // No global path sort: matches must stream in traversal order so a small
    // limit can stop the backend early instead of buffering the whole repo.
    assert!(
        !cmd.contains("--sort"),
        "search command must not globally sort: {cmd}"
    );
    // A second head stage emits one probe byte beyond the formal budget; the
    // parser consumes the probe only to prove truncation and never exposes it.
    assert!(
        cmd.contains(&format!("-c {}", SEARCH_OUTPUT_BYTE_BUDGET + 1)),
        "search command must cap output bytes with one probe byte: {cmd}"
    );
}

#[cfg(unix)]
fn write_executable_script(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

#[cfg(unix)]
fn fake_head_script() -> &'static str {
    "#!/bin/sh\nwhile IFS= read -r line; do\n  printf '%s\\n' \"$line\"\ndone\n"
}

/// Whether the host environment has a working real `rg` (ripgrep) on PATH.
///
/// Used only by integration tests that exercise advanced `search_project_text`
/// features against the installed backend. Fake-`rg` / controlled-PATH tests
/// must not call this — they supply their own backend.
fn host_ripgrep_available() -> bool {
    std::process::Command::new("rg")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn symlink_host_command(command: &str, bin: &std::path::Path) {
    let found = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {command}"))
        .output()
        .unwrap();
    assert!(found.status.success(), "host must provide {command}");
    let target = String::from_utf8(found.stdout).unwrap();
    let target = target.trim();
    assert!(!target.is_empty(), "host must provide {command}");
    std::os::unix::fs::symlink(target, bin.join(command)).unwrap();
}

#[cfg(unix)]
fn run_search_with_path(
    bin: &std::path::Path,
    root: &std::path::Path,
    options: &SearchOptions,
) -> ToolResult {
    let command = format!(
        "PATH={}; export PATH\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        search_project_text_command(options)
    );
    let (exit_code, stdout, stderr, _) = run_command_sync(&command, root, 10);
    assert_eq!(exit_code, 0, "stderr: {stderr}");
    let result = search_project_text_output("demo", options, &stdout, Some(exit_code), &stderr);
    assert!(result.success, "{:?}", result.error);
    result
}

#[cfg(unix)]
fn logical_search_matches(result: &ToolResult) -> Vec<(u64, String)> {
    let mut matches = result.output["matches"]
        .as_array()
        .expect("matches array")
        .iter()
        .map(|item| {
            (
                item["line"].as_u64().expect("match line"),
                item["preview"].as_str().expect("match preview").to_string(),
            )
        })
        .collect::<Vec<_>>();
    matches.sort();
    matches
}

#[cfg(unix)]
#[test]
fn search_project_text_command_prefers_rg_backend_when_available() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    write_executable_script(
        &bin.join("rg"),
        "#!/bin/sh\nprintf 'src/lib.rs:2:needle from rg\\n'\n",
    );
    write_executable_script(&bin.join("head"), fake_head_script());

    let cmd = format!(
        "PATH={}; export PATH\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        search_project_text_command(
            &SearchOptions::normalize(SearchRequest {
                limit: Some(5),
                ..raw_search_request()
            })
            .unwrap(),
        )
    );
    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, &root, 10);
    assert_eq!(exit_code, 0, "stderr: {stderr}");
    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(5),
        ..raw_search_request()
    })
    .unwrap();
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), "");

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["backend"], "rg");
    assert_eq!(result.output["truncated"], false);
    assert_eq!(result.output["matches"].as_array().unwrap().len(), 1);
    assert_eq!(result.output["matches"][0]["path"], "src/lib.rs");
    assert_eq!(result.output["matches"][0]["line"], 2);
    assert_eq!(result.output["matches"][0]["preview"], "needle from rg");
}

#[cfg(unix)]
#[test]
fn search_project_text_command_falls_back_to_grep_without_rg() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    write_executable_script(
        &bin.join("grep"),
        "#!/bin/sh\nprintf 'src/lib.rs:3:needle from grep\\n'\n",
    );
    write_executable_script(&bin.join("head"), fake_head_script());

    let cmd = format!(
        "PATH={}; export PATH\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        search_project_text_command(
            &SearchOptions::normalize(SearchRequest {
                limit: Some(5),
                ..raw_search_request()
            })
            .unwrap(),
        )
    );
    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, &root, 10);
    assert_eq!(exit_code, 0, "stderr: {stderr}");
    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(5),
        ..raw_search_request()
    })
    .unwrap();
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), "");

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["backend"], "grep");
    assert_eq!(result.output["truncated"], false);
    assert_eq!(result.output["matches"].as_array().unwrap().len(), 1);
    assert_eq!(result.output["matches"][0]["path"], "src/lib.rs");
    assert_eq!(result.output["matches"][0]["line"], 3);
    assert_eq!(result.output["matches"][0]["preview"], "needle from grep");
}

#[cfg(unix)]
#[test]
fn search_project_text_basic_regex_semantics_match_rg_and_grep_fallback() {
    if !host_ripgrep_available() {
        eprintln!("skipping regex backend-parity test: rg is unavailable");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let rg_bin = tmp.path().join("rg-bin");
    let grep_bin = tmp.path().join("grep-bin");
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&rg_bin).unwrap();
    std::fs::create_dir_all(&grep_bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("sample.txt"),
        "foo\nbar\nbaz\naaaa\na+\n(foo)\nfoo|bar\nRuntimeInfo {\n",
    )
    .unwrap();

    for command in ["rg", "grep", "head"] {
        symlink_host_command(command, &rg_bin);
    }
    for command in ["grep", "head"] {
        symlink_host_command(command, &grep_bin);
    }

    let cases = [
        (
            "foo|bar",
            vec![(1, "foo"), (2, "bar"), (6, "(foo)"), (7, "foo|bar")],
        ),
        (
            "a+",
            vec![
                (2, "bar"),
                (3, "baz"),
                (4, "aaaa"),
                (5, "a+"),
                (7, "foo|bar"),
            ],
        ),
        ("(foo)", vec![(1, "foo"), (6, "(foo)"), (7, "foo|bar")]),
        ("baz", vec![(3, "baz")]),
    ];

    for (pattern, expected) in cases {
        let options = SearchOptions::normalize_with_pattern_mode(
            SearchRequest {
                pattern: pattern.to_string(),
                limit: Some(20),
                ..raw_search_request()
            },
            Some(SearchPatternMode::Regex),
        )
        .unwrap();
        let command = search_project_text_command(&options);
        assert!(command.contains("grep -rnI --null -E"), "{command}");
        assert!(!command.contains("grep -rnI --null -F"), "{command}");

        let rg = run_search_with_path(&rg_bin, &root, &options);
        let grep = run_search_with_path(&grep_bin, &root, &options);
        assert_eq!(rg.output["backend"], "rg", "pattern {pattern}");
        assert_eq!(grep.output["backend"], "grep", "pattern {pattern}");
        assert_eq!(
            logical_search_matches(&rg),
            logical_search_matches(&grep),
            "pattern {pattern} must have backend-independent logical matches"
        );
        let expected = expected
            .into_iter()
            .map(|(line, preview)| (line, preview.to_string()))
            .collect::<Vec<_>>();
        assert_eq!(logical_search_matches(&grep), expected, "pattern {pattern}");
    }
}

#[cfg(unix)]
#[test]
fn search_project_text_grep_fallback_keeps_literal_patterns_literal() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("sample.txt"),
        "foo\nbar\nbaz\naaaa\na+\n(foo)\nfoo|bar\nRuntimeInfo {\n",
    )
    .unwrap();
    for command in ["grep", "head"] {
        symlink_host_command(command, &bin);
    }

    let cases = [
        ("foo|bar", 7, "foo|bar"),
        ("a+", 5, "a+"),
        ("(foo)", 6, "(foo)"),
        ("RuntimeInfo {", 8, "RuntimeInfo {"),
    ];
    for (pattern, line, preview) in cases {
        let options = SearchOptions::normalize_with_pattern_mode(
            SearchRequest {
                pattern: pattern.to_string(),
                limit: Some(20),
                ..raw_search_request()
            },
            Some(SearchPatternMode::Literal),
        )
        .unwrap();
        let command = search_project_text_command(&options);
        assert!(command.contains("grep -rnI --null -F"), "{command}");
        assert!(!command.contains("grep -rnI --null -E"), "{command}");

        let result = run_search_with_path(&bin, &root, &options);
        assert_eq!(result.output["backend"], "grep", "pattern {pattern}");
        assert_eq!(
            logical_search_matches(&result),
            vec![(line, preview.to_string())],
            "literal pattern {pattern}"
        );
    }
}

#[cfg(unix)]
#[test]
fn search_project_text_grep_fallback_excludes_ignored_claude_worktrees() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    let stale = root.join(".claude/worktrees/stale/src");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(root.join(".gitignore"), ".claude\n").unwrap();
    std::fs::write(root.join("src/live.txt"), "needle active\n").unwrap();
    std::fs::write(stale.join("old.txt"), "needle stale\n").unwrap();

    let git_init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(git_init.success());
    let git_ignore = std::process::Command::new("git")
        .args(["check-ignore", "-q", ".claude/worktrees/stale/src/old.txt"])
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(git_ignore.success(), "fixture must be ignored by Git");

    for command in ["grep", "head"] {
        let found = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {command}"))
            .output()
            .unwrap();
        assert!(found.status.success(), "host must provide {command}");
        let target = String::from_utf8(found.stdout).unwrap();
        let target = target.trim();
        assert!(!target.is_empty(), "host must provide {command}");
        std::os::unix::fs::symlink(target, bin.join(command)).unwrap();
    }

    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(10),
        ..raw_search_request()
    })
    .unwrap();
    let cmd = format!(
        "PATH={}; export PATH\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        search_project_text_command(&options)
    );
    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, &root, 10);
    assert_eq!(exit_code, 0, "stderr: {stderr}");
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), "");

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["backend"], "grep");
    let matches = result.output["matches"].as_array().unwrap();
    assert_eq!(
        matches.len(),
        1,
        "stale ignored worktree must not be searched"
    );
    assert_eq!(matches[0]["path"], "src/live.txt");
    assert_eq!(matches[0]["preview"], "needle active");
}

#[test]
fn parse_search_project_text_output_accepts_leading_canonical_marker_and_reports_limit_truncation()
{
    let stdout = concat!(
        "\n",
        " \r\n",
        "{\"webcodex_search\":{\"backend\":\"rg\",\"feature_unavailable\":false}}\n",
        "src/a.rs:1:needle one\n",
        "src/b.rs:2:needle two\n",
        "{\"webcodex_search\":{\"backend\":\"grep\"}}\n",
    );
    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(1),
        ..raw_search_request()
    })
    .unwrap();
    let result = search_project_text_output("demo", &options, stdout, Some(0), "");

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["backend"], "rg");
    assert_eq!(result.output["truncated"], true);
    assert_eq!(result.output["matches"].as_array().unwrap().len(), 1);
    assert_eq!(result.output["matches"][0]["path"], "src/a.rs");
}

fn raw_search_request() -> SearchRequest {
    SearchRequest {
        pattern: "needle".to_string(),
        path: None,
        limit: None,
        context_before: None,
        context_after: None,
        include_globs: None,
        exclude_globs: None,
        result_mode: None,
        timeout_secs: None,
    }
}

fn search_call(project: String, request: SearchRequest) -> ToolCall {
    ToolCall::SearchProjectTexts {
        project: project,
        queries: vec![crate::tool_runtime::SearchProjectTextsQuery {
            pattern: request.pattern,
            pattern_mode: None,
            path: request.path,
            limit: request.limit,
            context_before: request.context_before,
            context_after: request.context_after,
            include_globs: request.include_globs,
            exclude_globs: request.exclude_globs,
            result_mode: request.result_mode,
            timeout_secs: request.timeout_secs,
        }],
        session_id: None,
        max_result_bytes: None,
    }
}

fn extract_single_search_batch_result(batch: ToolResult) -> ToolResult {
    if !batch.success {
        return batch;
    }
    let items = batch.output["items"]
        .as_array()
        .expect("one-query search batch items");
    assert_eq!(items.len(), 1, "one-query search batch: {}", batch.output);
    let item = &items[0];
    ToolResult {
        success: item["success"]
            .as_bool()
            .expect("one-query search item success"),
        output: item.get("output").cloned().unwrap_or(Value::Null),
        error: item
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn assert_search_output_keys_are_declared(output: &Value) {
    if output.get("code").is_some() {
        return;
    }
    let schema = crate::tool_runtime::registry::output_schema_for_tool("search_project_texts");
    let item_output = &schema["properties"]["output"]["anyOf"][0]["anyOf"][0]["properties"]
        ["items"]["items"]["properties"]["output"];
    let mut declared = std::collections::BTreeSet::new();
    for variant in item_output["anyOf"][0]["anyOf"]
        .as_array()
        .expect("search success variants")
    {
        if let Some(properties) = variant["properties"].as_object() {
            declared.extend(properties.keys().cloned());
        }
    }
    if let Some(properties) = item_output["anyOf"][1]["properties"].as_object() {
        declared.extend(properties.keys().cloned());
    }
    let Some(output) = output.as_object() else {
        return;
    };
    for key in output.keys() {
        assert!(
            declared.contains(key),
            "runtime search output key {key} is not declared in search_project_texts item output schema"
        );
    }
}

async fn execute_agent_search(
    runtime: &ToolRuntime,
    client_id: &str,
    project: String,
    request: SearchRequest,
) -> (ToolResult, RunnerRequest) {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(search_call(project, request), Some(&bootstrap))
                .await
        }
    });
    let req = wait_for_patch_agent_request(runtime, client_id).await;
    let inspected = req.clone();
    complete_agent_request_by_running_locally(runtime, client_id, req).await;
    let result = extract_single_search_batch_result(task.await.unwrap());
    assert_search_output_keys_are_declared(&result.output);
    (result, inspected)
}

#[test]
fn search_options_default_to_compatible_matches_contract() {
    let options = SearchOptions::normalize(raw_search_request()).unwrap();

    assert_eq!(options.path, ".");
    assert_eq!(options.limit, 50);
    assert_eq!(options.context_before, 0);
    assert_eq!(options.context_after, 0);
    assert_eq!(options.result_mode, SearchResultMode::Matches);
    assert_eq!(options.timeout_secs, 30);
    assert!(options.include_globs.is_empty());
    assert!(options.exclude_globs.is_empty());
    assert!(!options.requires_ripgrep());
}

#[test]
fn search_options_clamp_timeout_and_context() {
    let mut low = raw_search_request();
    low.timeout_secs = Some(0);
    low.context_before = Some(usize::MAX);
    let low = SearchOptions::normalize(low).unwrap();
    assert_eq!(low.timeout_secs, 1);
    assert_eq!(low.context_before, MAX_SEARCH_CONTEXT_LINES);

    let high = SearchOptions::normalize(SearchRequest {
        timeout_secs: Some(999),
        context_after: Some(usize::MAX),
        ..raw_search_request()
    })
    .unwrap();
    assert_eq!(high.timeout_secs, 120);
    assert_eq!(high.context_after, MAX_SEARCH_CONTEXT_LINES);
}

#[test]
fn search_timeout_uses_structured_failure_with_effective_timeout() {
    let options = SearchOptions::normalize(SearchRequest {
        timeout_secs: Some(0),
        ..raw_search_request()
    })
    .unwrap();
    let result = search_project_text_output(
        "demo",
        &options,
        "{\"webcodex_search\":{\"backend\":\"rg\"}}\n",
        Some(-1),
        "Command timed out after 1 seconds",
    );

    assert!(!result.success);
    assert_search_output_keys_are_declared(&result.output);
    assert_eq!(result.output["code"], "search_timeout");
    assert_eq!(result.output["backend"], "rg");
    assert_eq!(result.output["effective_timeout_secs"], 1);
    assert_eq!(result.output["result_mode"], "matches");
}

#[test]
fn search_agent_timeout_budget_keeps_outer_above_command_at_max() {
    // At max configured timeout, shell-client wait is capped at 120 so it may
    // equal command timeout; outer wait must still exceed command timeout.
    let (command, wait, outer) = search_agent_timeout_budget(120);
    assert_eq!(command, 120);
    assert_eq!(wait, 120);
    assert!(outer > command, "outer={outer} command={command}");
    assert!(outer >= wait, "outer={outer} wait={wait}");

    let (command, wait, outer) = search_agent_timeout_budget(30);
    assert_eq!(command, 30);
    assert_eq!(wait, 32);
    assert_eq!(outer, 34);
    assert!(command < wait && wait < outer);

    let (command, wait, outer) = search_agent_timeout_budget(118);
    assert_eq!(command, 118);
    assert_eq!(wait, 120);
    assert!(command < wait);
    assert!(wait < outer || outer > command);
}

#[test]
fn search_backend_exit_codes_map_to_results() {
    enum Expect {
        // Exit 1 (no matches) is a successful empty result.
        EmptySuccess,
        // Exit 2 is a structured execution failure.
        ExecutionFailure,
        // Exit 0 parses the emitted match lines.
        ParsedMatches(usize),
    }
    struct Case {
        backend: &'static str,
        exit_code: i32,
        // Extra stdout line emitted between the two backend marker lines.
        match_line: Option<&'static str>,
        limit: Option<usize>,
        expect: Expect,
    }
    let cases = [
        Case {
            backend: "rg",
            exit_code: 1,
            match_line: None,
            limit: None,
            expect: Expect::EmptySuccess,
        },
        Case {
            backend: "rg",
            exit_code: 2,
            match_line: None,
            limit: None,
            expect: Expect::ExecutionFailure,
        },
        Case {
            backend: "rg",
            exit_code: 0,
            match_line: Some("src/lib.rs:2:needle from rg\n"),
            limit: Some(5),
            expect: Expect::ParsedMatches(1),
        },
        Case {
            backend: "grep",
            exit_code: 1,
            match_line: None,
            limit: None,
            expect: Expect::EmptySuccess,
        },
        Case {
            backend: "grep",
            exit_code: 2,
            match_line: None,
            limit: None,
            expect: Expect::ExecutionFailure,
        },
    ];

    for case in cases {
        let ctx = format!("backend={} exit_code={}", case.backend, case.exit_code);
        let options = SearchOptions::normalize(SearchRequest {
            limit: case.limit,
            ..raw_search_request()
        })
        .unwrap();
        let marker = format!(
            "{{\"webcodex_search\":{{\"backend\":\"{}\",\"feature_unavailable\":false}}}}\n",
            case.backend
        );
        let stdout = format!("{marker}{}{marker}", case.match_line.unwrap_or(""));
        let result =
            search_project_text_output("demo", &options, &stdout, Some(case.exit_code), "");
        assert_eq!(result.output["backend"], case.backend, "{ctx}");
        match case.expect {
            Expect::EmptySuccess => {
                assert!(result.success, "{ctx}: {:?}", result.error);
                assert_eq!(result.output["matches"], json!([]), "{ctx}");
                assert_eq!(result.output["count"], 0, "{ctx}");
                assert_eq!(result.output["exit_code"], case.exit_code, "{ctx}");
            }
            Expect::ExecutionFailure => {
                assert!(!result.success, "{ctx}");
                assert_search_output_keys_are_declared(&result.output);
                assert_eq!(result.output["code"], "search_execution_failed", "{ctx}");
                assert_eq!(result.output["failure_stage"], "backend_execution", "{ctx}");
                assert_eq!(
                    result.output["reason_code"], "backend_process_failed",
                    "{ctx}"
                );
                assert_eq!(result.output["exit_code"], case.exit_code, "{ctx}");
                assert_eq!(result.output["result_mode"], "matches", "{ctx}");
            }
            Expect::ParsedMatches(expected) => {
                assert!(result.success, "{ctx}: {:?}", result.error);
                assert_eq!(
                    result.output["matches"].as_array().unwrap().len(),
                    expected,
                    "{ctx}"
                );
            }
        }
    }
}

#[test]
fn search_markerless_output_cannot_be_reported_as_a_search_result() {
    let options = SearchOptions::normalize(raw_search_request()).unwrap();
    for (stdout, exit_code, stderr) in [
        (
            "",
            Some(1),
            "PowerShell parser rejected the generated POSIX search script",
        ),
        ("src/lib.rs:1:startup noise\n", Some(0), ""),
        ("src/lib.rs:1:partial output\n", Some(1), ""),
    ] {
        let result = search_project_text_output("demo", &options, stdout, exit_code, stderr);
        assert!(!result.success, "stdout={stdout:?} exit_code={exit_code:?}");
        assert_search_output_keys_are_declared(&result.output);
        assert_eq!(result.output["code"], "search_execution_failed");
        assert_eq!(result.output["failure_stage"], "backend_protocol");
        assert_eq!(result.output["reason_code"], "backend_identity_missing");
        assert!(result.output["backend"].is_null());
        let rendered = serde_json::to_string(&result).unwrap();
        assert!(!rendered.contains("PowerShell parser"));
        assert!(!rendered.contains("startup noise"));
    }
}

#[test]
fn search_backend_identity_requires_leading_canonical_namespaced_marker() {
    let options = SearchOptions::normalize(raw_search_request()).unwrap();
    for stdout in [
        "{\"backend\":\"rg\"}\nsrc/a.rs:1:needle\n",
        "src/z.rs:1:needle\n{\"webcodex_search\":{\"backend\":\"rg\"}}\n",
        "[output truncated to last 12000 bytes]\nsrc/z.rs:1:needle\n{\"webcodex_search\":{\"backend\":\"rg\"}}\n",
    ] {
        let result = search_project_text_output("demo", &options, stdout, Some(0), "");
        assert!(!result.success, "stdout={stdout:?}");
        assert_search_output_keys_are_declared(&result.output);
        assert_eq!(result.output["code"], "search_execution_failed");
        assert_eq!(result.output["failure_stage"], "backend_protocol");
        assert_eq!(result.output["reason_code"], "backend_identity_missing");
        assert!(result.output["backend"].is_null());
        let rendered = serde_json::to_string(&result).unwrap();
        assert!(!rendered.contains("src/a.rs"));
        assert!(!rendered.contains("src/z.rs"));
    }
}

#[test]
fn search_invalid_backend_marker_reports_protocol_provenance() {
    let options = SearchOptions::normalize(raw_search_request()).unwrap();
    for stdout in [
        "{\"webcodex_search\":{\"backend\":\"unknown\"}}\n",
        "{\"webcodex_search\":{\"backend\":\"rg\",\"feature_unavailable\":\"no\"}}\n",
        "{\"webcodex_search\":\n",
    ] {
        let result = search_project_text_output("demo", &options, stdout, Some(0), "");
        assert!(!result.success, "stdout={stdout:?}");
        assert_search_output_keys_are_declared(&result.output);
        assert_eq!(result.output["code"], "search_execution_failed");
        assert_eq!(result.output["failure_stage"], "backend_protocol");
        assert_eq!(result.output["reason_code"], "backend_identity_invalid");
        assert!(result.output["backend"].is_null());
    }
}

#[test]
fn search_missing_completion_status_cannot_prove_success() {
    let options = SearchOptions::normalize(raw_search_request()).unwrap();
    let stdout = "{\"webcodex_search\":{\"backend\":\"rg\",\"feature_unavailable\":false}}\n";
    let result = search_project_text_output("demo", &options, stdout, None, "");

    assert!(!result.success);
    assert_search_output_keys_are_declared(&result.output);
    assert_eq!(result.output["code"], "search_execution_failed");
    assert_eq!(result.output["failure_stage"], "backend_protocol");
    assert_eq!(result.output["reason_code"], "backend_status_unavailable");
    assert_eq!(result.output["backend"], "rg");
    assert!(result.output.get("exit_code").is_none());
}

#[test]
fn search_status_and_records_must_agree_before_empty_is_trusted() {
    let options = SearchOptions::normalize(raw_search_request()).unwrap();
    let marker = "{\"webcodex_search\":{\"backend\":\"rg\",\"feature_unavailable\":false}}\n";
    for (stdout, exit_code) in [
        (marker.to_string(), 0),
        (marker.to_string(), 141),
        (format!("{marker}src/lib.rs:1:needle\n"), 1),
        (format!("{marker}/private/absolute/secret.rs:1:needle\n"), 0),
    ] {
        let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), "");
        assert!(!result.success, "stdout={stdout:?} exit_code={exit_code}");
        assert_search_output_keys_are_declared(&result.output);
        assert_eq!(result.output["code"], "search_execution_failed");
        assert_eq!(result.output["failure_stage"], "backend_protocol");
        assert_eq!(result.output["reason_code"], "backend_output_inconsistent");
        assert_eq!(result.output["backend"], "rg");
        assert_eq!(result.output["exit_code"], exit_code);
        assert!(!serde_json::to_string(&result)
            .unwrap()
            .contains("/private/"));
    }

    let provider_marker =
        "{\"webcodex_search\":{\"backend\":\"claude_code\",\"feature_unavailable\":false}}\n";
    let unproven = search_project_text_output("demo", &options, provider_marker, Some(0), "");
    assert!(!unproven.success);
    assert_eq!(
        unproven.output["reason_code"],
        "backend_output_inconsistent"
    );
    let proven_empty = search_project_text_output("demo", &options, provider_marker, Some(1), "");
    assert!(proven_empty.success, "{:?}", proven_empty.error);
    assert_eq!(proven_empty.output["matches"], json!([]));
    assert_eq!(proven_empty.output["exit_code"], 1);
}

#[cfg(unix)]
#[test]
fn search_command_preserves_rg_exit_2_despite_head() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    // Fake rg always exits 2 (regex/execution error); head would mask this in a bare pipeline.
    write_executable_script(&bin.join("rg"), "#!/bin/sh\nexit 2\n");
    write_executable_script(&bin.join("head"), fake_head_script());

    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(5),
        ..raw_search_request()
    })
    .unwrap();
    let cmd = format!(
        "PATH={}; export PATH\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        search_project_text_command(&options)
    );
    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, &root, 10);
    assert_eq!(exit_code, 2, "stderr={stderr} stdout={stdout}");
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), &stderr);
    assert!(!result.success);
    assert_eq!(result.output["code"], "search_execution_failed");
    assert_eq!(result.output["backend"], "rg");
}

#[cfg(unix)]
#[test]
fn search_command_preserves_rg_exit_1_as_success_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    write_executable_script(&bin.join("rg"), "#!/bin/sh\nexit 1\n");
    write_executable_script(&bin.join("head"), fake_head_script());

    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(5),
        ..raw_search_request()
    })
    .unwrap();
    let cmd = format!(
        "PATH={}; export PATH\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        search_project_text_command(&options)
    );
    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, &root, 10);
    assert_eq!(exit_code, 1, "stderr={stderr} stdout={stdout}");
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), &stderr);
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["matches"], json!([]));
    assert_eq!(result.output["backend"], "rg");
}

#[cfg(unix)]
#[test]
fn search_command_preserves_grep_exit_2_despite_head() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    // No rg → grep path.
    write_executable_script(&bin.join("grep"), "#!/bin/sh\nexit 2\n");
    write_executable_script(&bin.join("head"), fake_head_script());

    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(5),
        ..raw_search_request()
    })
    .unwrap();
    let cmd = format!(
        "PATH={}; export PATH\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        search_project_text_command(&options)
    );
    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, &root, 10);
    assert_eq!(exit_code, 2, "stderr={stderr} stdout={stdout}");
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), &stderr);
    assert!(!result.success);
    assert_eq!(result.output["code"], "search_execution_failed");
    assert_eq!(result.output["backend"], "grep");
}

#[cfg(unix)]
#[test]
fn search_command_illegal_regex_is_not_swallowed_by_head() {
    // Real rg with an illegal regex should surface exit >= 2 through the generated shell.
    if !host_ripgrep_available() {
        eprintln!("skipping real-ripgrep integration test: rg is unavailable");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.txt"), "hello\n").unwrap();
    let options = SearchOptions::normalize(SearchRequest {
        pattern: "[invalid".to_string(),
        limit: Some(5),
        ..raw_search_request()
    })
    .unwrap();
    let (exit_code, stdout, stderr, _) =
        run_command_sync(&search_project_text_command(&options), &root, 10);
    assert!(
        exit_code >= 2,
        "illegal regex should fail backend: exit={exit_code} stderr={stderr} stdout={stdout}"
    );
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), &stderr);
    assert!(!result.success);
    assert_eq!(result.output["code"], "search_execution_failed");
    assert_eq!(result.output["failure_stage"], "backend_execution");
    assert_eq!(result.output["reason_code"], "backend_process_failed");
    assert_eq!(result.output["backend"], "rg");
    assert_eq!(result.output["exit_code"], exit_code);
    let rendered = serde_json::to_string(&result).unwrap();
    assert!(!rendered.contains("[invalid"));
}

#[cfg(unix)]
fn count_webcodex_search_status_files(dir: &std::path::Path) -> usize {
    let mut count = 0usize;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("webcodex-search-") {
            count += 1;
        }
        if entry.path().is_dir() {
            count += count_webcodex_search_status_files(&entry.path());
        }
    }
    count
}

#[cfg(unix)]
#[test]
fn search_status_tmpdir_relative_does_not_use_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    let rel_tmp = root.join("rel-status-tmp");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&rel_tmp).unwrap();
    write_executable_script(
        &bin.join("rg"),
        "#!/bin/sh\nprintf 'src/a.rs:1:needle\\n'\n",
    );
    write_executable_script(&bin.join("head"), fake_head_script());
    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(5),
        ..raw_search_request()
    })
    .unwrap();
    // Relative TMPDIR must fall back to /tmp — never create status files under project.
    let cmd = format!(
        "PATH={}; export PATH\nTMPDIR=rel-status-tmp; export TMPDIR\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        search_project_text_command(&options)
    );
    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, &root, 10);
    assert_eq!(exit_code, 0, "stderr={stderr} stdout={stdout}");
    assert_eq!(count_webcodex_search_status_files(&root), 0);
    assert_eq!(count_webcodex_search_status_files(&rel_tmp), 0);
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), &stderr);
    assert!(result.success, "{:?}", result.error);
}

#[cfg(unix)]
#[test]
fn search_status_tmpdir_project_root_does_not_use_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    let safe_tmp = tmp.path().join("safe-tmp");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&safe_tmp).unwrap();
    write_executable_script(
        &bin.join("rg"),
        "#!/bin/sh\nprintf 'src/a.rs:1:needle\\n'\n",
    );
    write_executable_script(&bin.join("head"), fake_head_script());
    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(5),
        ..raw_search_request()
    })
    .unwrap();
    // Absolute TMPDIR equal to project root is rejected; files must not land in worktree.
    let cmd = format!(
        "PATH={}; export PATH\nTMPDIR={}; export TMPDIR\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        shell_escape_simple(&root.to_string_lossy()),
        search_project_text_command(&options)
    );
    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, &root, 10);
    assert_eq!(exit_code, 0, "stderr={stderr} stdout={stdout}");
    assert_eq!(count_webcodex_search_status_files(&root), 0);
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), &stderr);
    assert!(result.success, "{:?}", result.error);
}

#[cfg(unix)]
#[test]
fn search_status_tmpdir_symlink_into_worktree_is_rejected() {
    // Outside symlink → inside worktree dir must not bypass physical-path checks.
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    let inside = root.join("inner-tmp");
    let outside_link = tmp.path().join("outside-link-to-inner");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&inside).unwrap();
    std::os::unix::fs::symlink(&inside, &outside_link).expect("create symlink");
    write_executable_script(
        &bin.join("rg"),
        "#!/bin/sh\nprintf 'src/a.rs:1:needle\\n'\n",
    );
    write_executable_script(&bin.join("head"), fake_head_script());
    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(5),
        ..raw_search_request()
    })
    .unwrap();
    let cmd = format!(
        "PATH={}; export PATH\nTMPDIR={}; export TMPDIR\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        shell_escape_simple(&outside_link.to_string_lossy()),
        search_project_text_command(&options)
    );
    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, &root, 10);
    assert_eq!(exit_code, 0, "stderr={stderr} stdout={stdout}");
    assert_eq!(
        count_webcodex_search_status_files(&root),
        0,
        "symlink-into-worktree TMPDIR must not create status files under the project"
    );
    assert_eq!(count_webcodex_search_status_files(&inside), 0);
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), &stderr);
    assert!(result.success, "{:?}", result.error);
}

#[cfg(unix)]
#[test]
fn search_status_file_is_removed_after_successful_run() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    let safe_tmp = tmp.path().join("safe-tmp");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&safe_tmp).unwrap();
    write_executable_script(
        &bin.join("rg"),
        "#!/bin/sh\nprintf 'src/a.rs:1:needle\\n'\n",
    );
    write_executable_script(&bin.join("head"), fake_head_script());
    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(5),
        ..raw_search_request()
    })
    .unwrap();
    let cmd = format!(
        "PATH={}; export PATH\nTMPDIR={}; export TMPDIR\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        shell_escape_simple(&safe_tmp.to_string_lossy()),
        search_project_text_command(&options)
    );
    let before = count_webcodex_search_status_files(&safe_tmp);
    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, &root, 10);
    assert_eq!(exit_code, 0, "stderr={stderr} stdout={stdout}");
    let after = count_webcodex_search_status_files(&safe_tmp);
    assert_eq!(before, 0);
    assert_eq!(after, 0, "status files must be cleaned after success");
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), &stderr);
    assert!(result.success, "{:?}", result.error);
}

#[cfg(unix)]
#[test]
fn search_early_stop_reaps_process_group_and_status_files() {
    // An infinite fake rg is stopped early by the head budget. The whole
    // wrapper process group (rg + the two head stages + the wrapper shell)
    // must be reaped and the status file removed — nothing is left behind to
    // be cleaned up by a later request.
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    let safe_tmp = tmp.path().join("safe-tmp");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&safe_tmp).unwrap();
    write_executable_script(
        &bin.join("rg"),
        "#!/bin/sh\ni=0\nwhile :; do\n  printf 'src/f%d.rs:%d:needle line\\n' \"$i\" \"$i\"\n  i=$((i + 1))\ndone\n",
    );
    write_executable_script(&bin.join("head"), truncating_fake_head_script());
    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(3),
        ..raw_search_request()
    })
    .unwrap();
    let cmd = format!(
        "PATH={}; export PATH\nTMPDIR={}; export TMPDIR\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        shell_escape_simple(&safe_tmp.to_string_lossy()),
        search_project_text_command(&options)
    );
    let before = count_webcodex_search_status_files(&safe_tmp);
    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, &root, 10);
    let after = count_webcodex_search_status_files(&safe_tmp);
    assert_eq!(before, 0);
    assert_eq!(after, 0, "status files must be cleaned after early stop");
    // Exit 141 = the backend was SIGPIPEd by the head budget, an intentional
    // early stop, not a failure.
    assert_eq!(exit_code, 141, "stderr={stderr} stdout={stdout}");
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), &stderr);
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["matches"].as_array().unwrap().len(), 3);
    assert_eq!(result.output["truncation_reason"], "limit");
}

#[cfg(unix)]
#[test]
fn search_status_cleanup_trap_removes_file_on_term() {
    // Mirrors production cleanup_search_status + signal trap; verifies TERM path
    // without a long sleep. File removal is the contract.
    let tmp = tempfile::tempdir().unwrap();
    let status = tmp.path().join("webcodex-search-term-test");
    let script = format!(
        r#"status_file={path}
cleanup_search_status() {{
  if [ -n "${{status_file:-}}" ]; then
    /bin/rm -f "$status_file" 2>/dev/null || /usr/bin/rm -f "$status_file" 2>/dev/null || rm -f "$status_file" 2>/dev/null || true
    status_file=
  fi
}}
trap 'cleanup_search_status' EXIT
trap 'cleanup_search_status; exit 143' HUP INT TERM
: > "$status_file"
kill -s TERM $$
exit 1
"#,
        path = shell_escape_simple(&status.to_string_lossy())
    );
    let (exit_code, _stdout, stderr, _) = run_command_sync(&script, tmp.path(), 5);
    assert!(
        !status.exists(),
        "TERM trap must remove status file; exit={exit_code} stderr={stderr}"
    );
}

#[test]
fn resolve_search_head_command_prefers_path_then_absolute() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    #[cfg(unix)]
    {
        write_executable_script(&bin.join("head"), fake_head_script());
        let path = format!("{}:/usr/bin", bin.display());
        let resolved = resolve_search_head_command(Some(&path), &["/usr/bin/head", "/bin/head"])
            .expect("path head");
        assert!(resolved.contains("bin"), "{resolved}");
        assert!(resolved.ends_with("head"), "{resolved}");
    }
    let missing = resolve_search_head_command(Some("/nonexistent/path/for/head"), &[]);
    assert!(missing.is_none());
    let absolute = resolve_search_head_command(
        Some("/nonexistent/path/for/head"),
        DEFAULT_SEARCH_HEAD_ABSOLUTE_CANDIDATES,
    );
    // System may or may not have /usr/bin/head; when present, absolute resolves.
    if std::path::Path::new("/usr/bin/head").is_file()
        || std::path::Path::new("/bin/head").is_file()
    {
        assert!(absolute.is_some());
    }
}

#[cfg(unix)]
#[test]
fn search_command_fails_when_head_unavailable() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    write_executable_script(
        &bin.join("rg"),
        "#!/bin/sh\nprintf 'src/a.rs:1:needle\\n'\n",
    );
    // No head in PATH and no absolute fallbacks → fail closed (no unbounded output).
    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(5),
        ..raw_search_request()
    })
    .unwrap();
    let cmd = format!(
        "PATH={}; export PATH\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        search_project_text_command_with_head_fallbacks(&options, &[])
    );
    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, &root, 10);
    assert_eq!(exit_code, 2, "stderr={stderr} stdout={stdout}");
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), &stderr);
    assert!(!result.success);
    assert_eq!(result.output["code"], "search_execution_failed");
}

#[cfg(unix)]
#[test]
fn search_command_fails_when_head_exits_nonzero_even_if_backend_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    write_executable_script(
        &bin.join("rg"),
        "#!/bin/sh\nprintf 'src/a.rs:1:needle\\n'\n",
    );
    write_executable_script(&bin.join("head"), "#!/bin/sh\nexit 2\n");
    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(5),
        ..raw_search_request()
    })
    .unwrap();
    // Prefer PATH head (broken) over absolute system head by using empty absolute fallbacks.
    let cmd = format!(
        "PATH={}; export PATH\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        search_project_text_command_with_head_fallbacks(&options, &[])
    );
    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, &root, 10);
    assert_eq!(exit_code, 2, "stderr={stderr} stdout={stdout}");
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), &stderr);
    assert!(!result.success);
    assert_eq!(result.output["code"], "search_execution_failed");
}

#[cfg(unix)]
#[test]
fn search_command_keeps_success_when_head_is_available() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    write_executable_script(
        &bin.join("rg"),
        "#!/bin/sh\nprintf 'src/a.rs:1:needle\\n'\n",
    );
    write_executable_script(&bin.join("head"), fake_head_script());
    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(5),
        ..raw_search_request()
    })
    .unwrap();
    let cmd = format!(
        "PATH={}; export PATH\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        search_project_text_command(&options)
    );
    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, &root, 10);
    assert_eq!(exit_code, 0, "stderr={stderr}");
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), &stderr);
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["matches"].as_array().unwrap().len(), 1);
}

#[cfg(unix)]
/// A `head` that actually bounds output: stops after `-n <count>` lines or
/// `-c <count>` bytes. The shared `fake_head_script()` is a passthrough that
/// never closes the pipe, which is exactly what the early-stop tests must not
/// use — they need the pipe to close so the backend is SIGPIPEd.
fn truncating_fake_head_script() -> &'static str {
    r#"#!/bin/sh
n=1000000
c=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    -n) n=$2; shift 2 ;;
    -c) c=$2; shift 2 ;;
    *) shift ;;
  esac
done
count=0
bytes=0
while IFS= read -r line; do
  count=$((count + 1))
  bytes=$((bytes + ${#line} + 1))
  if [ "$c" -gt 0 ] && [ "$bytes" -gt "$c" ]; then
    exit 0
  fi
  printf '%s\n' "$line"
  if [ "$count" -ge "$n" ]; then
    exit 0
  fi
done
"#
}

#[cfg(unix)]
#[test]
fn search_small_limit_stops_unbounded_backend_early() {
    // Fake rg emits a never-ending stream of matches and never exits on its
    // own. A small limit must close the pipe and return promptly with exactly
    // the requested records; without early stop this would run until the
    // command timeout. Deterministic: no reliance on machine speed — the fake
    // either keeps streaming (and the truncating head closes it) or the test
    // times out.
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    write_executable_script(
        &bin.join("rg"),
        r#"#!/bin/sh
i=0
while :; do
  printf 'src/file%d.rs:%d:needle line\n' "$i" "$i"
  i=$((i + 1))
done
"#,
    );
    write_executable_script(&bin.join("head"), truncating_fake_head_script());
    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(3),
        ..raw_search_request()
    })
    .unwrap();
    let cmd = format!(
        "PATH={}; export PATH\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        search_project_text_command(&options)
    );
    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, &root, 10);
    assert_eq!(exit_code, 141, "stderr={stderr} stdout={stdout}");
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), &stderr);
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["matches"].as_array().unwrap().len(), 3);
    assert_eq!(result.output["truncated"], true);
    assert_eq!(result.output["truncation_reason"], "limit");
    // The fake backend was stopped early: the output is bounded even though
    // the producer was infinite.
    assert!(
        stdout.len() < 4096,
        "stdout unexpectedly large: {}",
        stdout.len()
    );
}

#[cfg(unix)]
#[test]
fn search_overlong_match_line_does_not_overflow_byte_budget() {
    // A single match line far longer than the byte budget must be truncated by
    // the `head -c` stage, and the parser must return only complete records
    // with truncation_reason = "output_bytes" instead of surfacing a half
    // record. The fake `head` delegates to the real system head so the byte
    // boundary cut is byte-accurate and deterministic.
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    // Fake rg emits one small complete match followed by a single enormous
    // match line that itself exceeds the byte budget; it then exits cleanly.
    // Pure shell so it needs nothing from the restricted PATH.
    write_executable_script(
        &bin.join("rg"),
        "#!/bin/sh\n\
         printf 'src/small.rs:1:needle ok\\n'\n\
         printf 'src/big.rs:2:'\n\
         i=0\n\
         while [ \"$i\" -lt 200000 ]; do printf 'x'; i=$((i + 1)); done\n\
         printf '\\n'\n\
         exit 0\n",
    );
    // Real-head semantics: byte-accurate -n/-c truncation. The restricted PATH
    // contains only this delegating head plus the fake rg.
    write_executable_script(&bin.join("head"), "#!/bin/sh\nexec /usr/bin/head \"$@\"\n");
    let options = SearchOptions::normalize(SearchRequest {
        limit: Some(5),
        ..raw_search_request()
    })
    .unwrap();
    let cmd = format!(
        "PATH={}; export PATH\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        search_project_text_command(&options)
    );
    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, &root, 10);
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), &stderr);

    assert!(result.success, "{:?}", result.error);
    // The small complete record survived; the over-long record was cut by the
    // byte budget and its partial tail dropped.
    let matches = result.output["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["path"], "src/small.rs");
    assert_eq!(result.output["truncated"], true);
    assert_eq!(result.output["truncation_reason"], "output_bytes");
    // No half record is surfaced, and the raw stdout (marker + budget) stays
    // bounded: the budget bytes plus the small leading marker.
    assert!(
        stdout.len() <= SEARCH_OUTPUT_BYTE_BUDGET + 256,
        "stdout={} exceeds byte budget",
        stdout.len()
    );
}

#[test]
fn search_requires_ripgrep_based_on_effective_features_not_field_presence() {
    let empty_globs = SearchOptions::normalize(SearchRequest {
        include_globs: Some(vec![]),
        exclude_globs: Some(vec![]),
        timeout_secs: Some(5),
        ..raw_search_request()
    })
    .unwrap();
    assert!(!empty_globs.requires_ripgrep());
    assert!(empty_globs.include_globs.is_empty());
    assert!(empty_globs.exclude_globs.is_empty());
    assert_eq!(empty_globs.timeout_secs, 5);

    let timeout_only = SearchOptions::normalize(SearchRequest {
        timeout_secs: Some(5),
        result_mode: Some(SearchResultMode::Matches),
        ..raw_search_request()
    })
    .unwrap();
    assert!(!timeout_only.requires_ripgrep());

    let with_include = SearchOptions::normalize(SearchRequest {
        include_globs: Some(vec!["**/*.rs".to_string()]),
        ..raw_search_request()
    })
    .unwrap();
    assert!(with_include.requires_ripgrep());

    let count_mode = SearchOptions::normalize(SearchRequest {
        result_mode: Some(SearchResultMode::Count),
        ..raw_search_request()
    })
    .unwrap();
    assert!(count_mode.requires_ripgrep());
}

#[test]
fn search_validation_errors_are_structured_without_raw_secrets() {
    let secret_pattern = "SUPER_SECRET_PATTERN_VALUE_XYZ";
    let secret_glob = "private-secret-name/**/*.rs";

    let empty = SearchOptions::normalize(SearchRequest {
        pattern: "   ".to_string(),
        ..raw_search_request()
    })
    .unwrap_err();
    assert_eq!(empty.field, "pattern");
    assert_eq!(empty.reason, Some("empty"));

    let nul = SearchOptions::normalize(SearchRequest {
        pattern: format!("a{secret_pattern}\0b"),
        ..raw_search_request()
    })
    .unwrap_err();
    assert_eq!(nul.field, "pattern");
    assert!(!nul.message.contains(secret_pattern));

    let path = SearchOptions::normalize(SearchRequest {
        path: Some("../outside".to_string()),
        ..raw_search_request()
    })
    .unwrap_err();
    assert_eq!(path.field, "path");

    let glob = SearchOptions::normalize(SearchRequest {
        include_globs: Some(vec![format!("!{secret_glob}")]),
        ..raw_search_request()
    })
    .unwrap_err();
    assert_eq!(glob.field, "include_globs");
    assert_eq!(glob.reason, Some("negated"));
    assert_eq!(glob.index, Some(0));
    assert!(!glob.message.contains(secret_glob));
}

#[tokio::test]
async fn search_invalid_request_dispatch_returns_structured_error() {
    // Authorization runs before the tool body; register shell capability so
    // normalize validation is reached and returns structured output.
    let runtime = runtime_with_agent_project("search-invalid");
    register_agent(
        &runtime,
        "search-invalid",
        None,
        RunnerCapabilities {
            shell: true,
            ..Default::default()
        },
    )
    .await;
    let secret_glob = "NEVER_ECHO_THIS_GLOB_VALUE/**";
    let result = runtime
        .dispatch_with_auth(
            search_call(
                agent_test_project_id("search-invalid"),
                SearchRequest {
                    include_globs: Some(vec![format!("!{secret_glob}")]),
                    ..raw_search_request()
                },
            ),
            Some(&auth_context(None, true)),
        )
        .await;
    let result = extract_single_search_batch_result(result);

    // Validation fails before any agent search request is enqueued.
    assert!(!result.success);
    assert_search_output_keys_are_declared(&result.output);
    assert_eq!(result.output["error_kind"], "search_project_text_failed");
    assert_eq!(result.output["failure_stage"], "request_validation");
    assert_eq!(result.output["reason_code"], "invalid_glob");
    assert_eq!(result.output["detail_code"], "invalid_glob");
    assert_eq!(result.output["state_changed"], false);
    let rendered = serde_json::to_string(&result.output).unwrap();
    assert!(!rendered.contains("NEVER_ECHO_THIS_GLOB_VALUE"));
    assert!(!result.error.as_deref().unwrap_or("").contains("NEVER_ECHO"));
}

#[tokio::test]
async fn search_agent_command_timeout_returns_search_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "needle\n").unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "search-cmd-timeout", "demo", tmp.path()).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    search_call(
                        project,
                        SearchRequest {
                            timeout_secs: Some(1),
                            ..raw_search_request()
                        },
                    ),
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "search-cmd-timeout").await;
    assert_eq!(req.timeout_secs, 1);
    // Simulate Runner-side command timeout response (lowercase message + error field).
    runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: "search-cmd-timeout".to_string(),
            runner_instance_id: "inst".to_string(),
            request_id: req.request_id,
            exit_code: Some(-1),
            stdout: Some(
                "{\"webcodex_search\":{\"backend\":\"rg\",\"feature_unavailable\":false}}\n"
                    .to_string(),
            ),
            stderr: Some("command timed out after 1 seconds".to_string()),
            duration_ms: Some(1000),
            error: Some("command timed out".to_string()),
        })
        .await
        .unwrap();
    let result = extract_single_search_batch_result(task.await.unwrap());
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "search_project_text_failed");
    assert_eq!(result.output["failure_stage"], "backend_execution");
    assert_eq!(result.output["reason_code"], "timeout");
    assert_eq!(result.output["detail_code"], "timeout");
    assert_eq!(result.output["state_changed"], false);
    assert_search_output_keys_are_declared(&result.output);
    assert_eq!(result.output["result_mode"], "matches");
    assert_eq!(result.output["effective_timeout_secs"], 1);
    assert_eq!(result.output["backend"], "rg");
}

#[tokio::test]
async fn search_agent_execution_failure_is_structured_and_does_not_leak_diagnostics() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "search-agent-failure", "demo", tmp.path()).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    search_call(project, raw_search_request()),
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "search-agent-failure").await;
    let private_diagnostic =
        "provider private prose at /private/runner/workspace with token=NEVER_RETURN";
    runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: "search-agent-failure".to_string(),
            runner_instance_id: "inst".to_string(),
            request_id: req.request_id,
            exit_code: Some(9),
            stdout: Some(
                "{\"webcodex_search\":{\"backend\":\"rg\",\"feature_unavailable\":false}}\n"
                    .to_string(),
            ),
            stderr: Some(private_diagnostic.to_string()),
            duration_ms: Some(5),
            error: Some(private_diagnostic.to_string()),
        })
        .await
        .unwrap();

    let result = extract_single_search_batch_result(task.await.unwrap());
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "search_project_text_failed");
    assert_eq!(result.output["failure_stage"], "agent_execution");
    assert_eq!(result.output["reason_code"], "search_execution_failed");
    assert_eq!(result.output["detail_code"], "agent_execution_failed");
    assert_eq!(result.output["state_changed"], false);
    assert_search_output_keys_are_declared(&result.output);
    assert_eq!(result.output["backend"], "rg");
    assert_eq!(result.output["exit_code"], 9);
    let rendered = serde_json::to_string(&result).unwrap();
    assert!(!rendered.contains("private prose"));
    assert!(!rendered.contains("/private/"));
    assert!(!rendered.contains("NEVER_RETURN"));
}

#[tokio::test]
async fn search_agent_timeout_without_trusted_marker_cannot_return_partial_success() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "search-timeout-no-marker", "demo", tmp.path())
            .await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    search_call(
                        project,
                        SearchRequest {
                            timeout_secs: Some(1),
                            ..raw_search_request()
                        },
                    ),
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "search-timeout-no-marker").await;
    runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: "search-timeout-no-marker".to_string(),
            runner_instance_id: "inst".to_string(),
            request_id: req.request_id,
            exit_code: Some(-1),
            stdout: Some("src/a.rs:1:needle\n".to_string()),
            stderr: Some("command timed out after 1 seconds".to_string()),
            duration_ms: Some(1000),
            error: Some("command timed out".to_string()),
        })
        .await
        .unwrap();

    let result = extract_single_search_batch_result(task.await.unwrap());
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "search_project_text_failed");
    assert_eq!(result.output["failure_stage"], "agent_execution");
    assert_eq!(result.output["reason_code"], "timeout");
    assert_eq!(result.output["detail_code"], "timeout");
    assert_eq!(result.output["state_changed"], false);
    assert_search_output_keys_are_declared(&result.output);
    assert!(result.output["backend"].is_null());
    assert!(result.output.get("matches").is_none());
}

#[tokio::test]
async fn search_agent_timeout_with_complete_records_returns_partial_success() {
    // Local and agent paths share the same parser, so an agent-reported
    // timeout that arrived with complete records returns the same partial
    // success semantics as the local path.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "needle\n").unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "search-ptimeout", "demo", tmp.path()).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    search_call(
                        project,
                        SearchRequest {
                            timeout_secs: Some(1),
                            ..raw_search_request()
                        },
                    ),
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "search-ptimeout").await;
    runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: "search-ptimeout".to_string(),
            runner_instance_id: "inst".to_string(),
            request_id: req.request_id,
            exit_code: Some(-1),
            stdout: Some(
                "{\"webcodex_search\":{\"backend\":\"rg\",\"feature_unavailable\":false}}\n\
                 src/a.rs:1:needle\n"
                    .to_string(),
            ),
            stderr: Some("command timed out after 1 seconds".to_string()),
            duration_ms: Some(1000),
            error: Some("command timed out".to_string()),
        })
        .await
        .unwrap();
    let result = extract_single_search_batch_result(task.await.unwrap());
    assert!(result.success, "{:?}", result.error);
    assert_search_output_keys_are_declared(&result.output);
    assert_eq!(result.output["backend"], "rg");
    assert_eq!(result.output["result_mode"], "matches");
    assert_eq!(result.output["effective_timeout_secs"], 1);
    assert_eq!(result.output["truncated"], true);
    assert_eq!(result.output["truncation_reason"], "timeout");
    let matches = result.output["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["path"], "src/a.rs");
    assert_eq!(matches[0]["line"], 1);
    assert_eq!(matches[0]["preview"], "needle");
}

#[tokio::test]
async fn search_agent_outer_timeout_returns_search_timeout_and_cancels() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "needle\n").unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "search-outer-timeout", "demo", tmp.path()).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    search_call(
                        project,
                        SearchRequest {
                            // Outer wait is command+4 seconds; leave request unanswered.
                            timeout_secs: Some(1),
                            ..raw_search_request()
                        },
                    ),
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "search-outer-timeout").await;
    let request_id = req.request_id.clone();
    assert_eq!(req.timeout_secs, 1);
    // Do not complete the agent request; outer tokio timeout should fire.
    let result = extract_single_search_batch_result(task.await.unwrap());
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "search_project_text_failed");
    assert_eq!(result.output["failure_stage"], "agent_transport");
    assert_eq!(result.output["reason_code"], "timeout");
    assert_eq!(result.output["detail_code"], "timeout");
    assert_eq!(result.output["state_changed"], false);
    assert_search_output_keys_are_declared(&result.output);
    assert_eq!(result.output["result_mode"], "matches");
    assert_eq!(result.output["effective_timeout_secs"], 1);
    assert!(
        result.output.get("backend").is_none() || result.output["backend"].is_null(),
        "outer timeout should not invent backend: {}",
        result.output
    );
    // Request should have been cancelled (no longer pending for completion).
    let complete = runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: "search-outer-timeout".to_string(),
            runner_instance_id: "inst".to_string(),
            request_id,
            exit_code: Some(0),
            stdout: Some(String::new()),
            stderr: Some(String::new()),
            duration_ms: Some(1),
            error: None,
        })
        .await;
    assert!(
        complete.is_err(),
        "cancelled request should reject late complete: {complete:?}"
    );
}

#[tokio::test]
async fn search_agent_request_dropped_returns_structured_error() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "needle\n").unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "search-dropped", "demo", tmp.path()).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    search_call(
                        project,
                        SearchRequest {
                            timeout_secs: Some(30),
                            ..raw_search_request()
                        },
                    ),
                    Some(&bootstrap),
                )
                .await
        }
    });
    let first = wait_for_patch_agent_request(&runtime, "search-dropped").await;
    // A one-query canonical batch retries one dropped Runner request once.
    runtime
        .runner_registry
        .cancel_request(&first.request_id)
        .await;
    let second = wait_for_patch_agent_request(&runtime, "search-dropped").await;
    runtime
        .runner_registry
        .cancel_request(&second.request_id)
        .await;
    let result = extract_single_search_batch_result(task.await.unwrap());
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "search_project_text_failed");
    assert_eq!(result.output["failure_stage"], "agent_transport");
    assert_eq!(result.output["reason_code"], "search_request_dropped");
    assert_eq!(result.output["detail_code"], "search_request_dropped");
    assert_eq!(result.output["state_changed"], false);
    assert_search_output_keys_are_declared(&result.output);
    assert_eq!(result.output["result_mode"], "matches");
    let effective_timeout = result.output["effective_timeout_secs"].as_u64().unwrap();
    assert!((29..=30).contains(&effective_timeout));
    assert!(
        result.error.as_deref().unwrap_or("").contains("dropped"),
        "{:?}",
        result.error
    );
}

#[cfg(unix)]
#[tokio::test]
async fn search_timeout_only_without_rg_still_allows_grep_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(root.join("a.rs"), "timeout_fallback_needle\n").unwrap();
    write_executable_script(
        &bin.join("grep"),
        "#!/bin/sh\nprintf 'a.rs:1:timeout_fallback_needle\\n'\n",
    );
    write_executable_script(&bin.join("head"), fake_head_script());

    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "search-timeout-fallback", "demo", &root).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    search_call(
                        project,
                        SearchRequest {
                            pattern: "timeout_fallback_needle".to_string(),
                            result_mode: Some(SearchResultMode::Matches),
                            timeout_secs: Some(5),
                            ..raw_search_request()
                        },
                    ),
                    Some(&bootstrap),
                )
                .await
        }
    });
    let mut req = wait_for_patch_agent_request(&runtime, "search-timeout-fallback").await;
    // Force grep path (no rg in PATH).
    req.command = format!(
        "PATH={}; export PATH\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        req.command
    );
    assert_eq!(req.timeout_secs, 5);
    complete_agent_request_by_running_locally(&runtime, "search-timeout-fallback", req).await;
    let result = extract_single_search_batch_result(task.await.unwrap());
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["backend"], "grep");
    assert_eq!(result.output["effective_timeout_secs"], 5);
    assert_eq!(result.output["matches"].as_array().unwrap().len(), 1);
}

#[test]
fn search_glob_validation_rejects_invalid_and_protected_inputs() {
    let invalid = [
        "",
        "!**/*.rs",
        "docs/\n*.md",
        "docs/\t*.md",
        "docs/\0*.md",
        "**/.env",
        "secrets/**",
        "**/*.key",
    ];
    for glob in invalid {
        let result = SearchOptions::normalize(SearchRequest {
            include_globs: Some(vec![glob.to_string()]),
            ..raw_search_request()
        });
        assert!(result.is_err(), "include glob {glob:?} should be rejected");
        let err = result.unwrap_err();
        assert_eq!(err.field, "include_globs");
        assert!(!err.message.contains(glob) || glob.is_empty());
    }

    for glob in ["", "!vendor/**", "vendor/\n**"] {
        let result = SearchOptions::normalize(SearchRequest {
            exclude_globs: Some(vec![glob.to_string()]),
            ..raw_search_request()
        });
        assert!(result.is_err(), "exclude glob {glob:?} should be rejected");
        assert_eq!(result.unwrap_err().field, "exclude_globs");
    }
}

#[test]
fn search_glob_validation_enforces_count_and_byte_limits() {
    let too_many = (0..=MAX_SEARCH_GLOBS)
        .map(|index| format!("src/{index}/**"))
        .collect::<Vec<_>>();
    let count_error = SearchOptions::normalize(SearchRequest {
        include_globs: Some(too_many),
        ..raw_search_request()
    })
    .unwrap_err();
    assert_eq!(count_error.field, "include_globs");
    assert_eq!(count_error.reason, Some("too_many"));
    assert!(
        count_error.message.contains("at most 32"),
        "{}",
        count_error.message
    );

    let length_error = SearchOptions::normalize(SearchRequest {
        exclude_globs: Some(vec!["a".repeat(MAX_SEARCH_GLOB_BYTES + 1)]),
        ..raw_search_request()
    })
    .unwrap_err();
    assert_eq!(length_error.field, "exclude_globs");
    assert_eq!(length_error.reason, Some("too_long"));
    assert_eq!(length_error.index, Some(0));
    assert!(
        length_error.message.contains("256 bytes"),
        "{}",
        length_error.message
    );
}

#[test]
fn search_audit_arguments_record_bounded_feature_summary_without_pattern_or_globs() {
    let raw = json!({
        "project": "agent:demo:project",
        "queries": [{
            "pattern": "NEVER_LOG_PATTERN_VALUE",
            "path": "src",
            "limit": 7,
            "context_before": 1,
            "context_after": 2,
            "include_globs": ["private name/**/*.rs"],
            "exclude_globs": ["generated secret name/**"],
            "result_mode": "count",
            "timeout_secs": 45
        }]
    });
    let raw_summary = super::super::tool_audit::session_log_arguments_for_tool_request(
        "search_project_texts",
        &raw,
    );
    assert_eq!(raw_summary["query_count"], 1);
    assert_eq!(raw_summary["patterns_present"], true);
    let raw_json = serde_json::to_string(&raw_summary).unwrap();
    assert!(!raw_json.contains("NEVER_LOG_PATTERN_VALUE"));
    assert!(!raw_json.contains("private name"));
    assert!(!raw_json.contains("generated secret name"));

    let call_summary = search_call(
        "agent:demo:project".to_string(),
        SearchRequest {
            pattern: "NEVER_LOG_PATTERN_VALUE".to_string(),
            include_globs: Some(vec!["private name/**/*.rs".to_string()]),
            exclude_globs: Some(vec!["generated secret name/**".to_string()]),
            result_mode: Some(SearchResultMode::Count),
            timeout_secs: Some(45),
            ..raw_search_request()
        },
    )
    .session_log_arguments();
    assert_eq!(call_summary["query_count"], 1);
    assert_eq!(call_summary["patterns_present"], true);
    let call_json = serde_json::to_string(&call_summary).unwrap();
    assert!(!call_json.contains("NEVER_LOG_PATTERN_VALUE"));
    assert!(!call_json.contains("private name"));
    assert!(!call_json.contains("generated secret name"));
}

#[cfg(unix)]
#[test]
fn search_command_passes_shell_metacharacter_globs_as_one_literal_argument() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    write_executable_script(
        &bin.join("rg"),
        "#!/bin/sh\nfor arg do printf 'ARG=%s\\n' \"$arg\"; done\n",
    );
    write_executable_script(&bin.join("head"), fake_head_script());
    let literal = "src/**/space $HOME; 'double\" `tick`";
    let options = SearchOptions::normalize(SearchRequest {
        include_globs: Some(vec![literal.to_string()]),
        ..raw_search_request()
    })
    .unwrap();
    let cmd = format!(
        "PATH={}; export PATH\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        search_project_text_command(&options)
    );

    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, &root, 10);
    assert_eq!(exit_code, 0, "stderr: {stderr}");
    assert!(stdout.contains(&format!("ARG={literal}")), "{stdout}");
    assert!(
        stdout.contains("$HOME"),
        "environment expansion leaked into argv: {stdout}"
    );
    assert!(
        !stdout.contains("\ndouble\" `tick`\n"),
        "glob split into a command: {stdout}"
    );
}

#[tokio::test]
async fn search_project_text_include_and_exclude_globs_are_additive() {
    // include/exclude globs are ripgrep-only; without host rg this is a
    // capability error, not a product regression (see
    // advanced_search_without_rg_returns_structured_capability_error).
    if !host_ripgrep_available() {
        eprintln!("skipping real-ripgrep integration test: rg is unavailable");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::create_dir_all(tmp.path().join("docs")).unwrap();
    std::fs::create_dir_all(tmp.path().join("vendor")).unwrap();
    std::fs::create_dir_all(tmp.path().join("secrets")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), "SCOPE_NEEDLE\n").unwrap();
    std::fs::write(tmp.path().join("docs/guide.md"), "SCOPE_NEEDLE\n").unwrap();
    std::fs::write(tmp.path().join("vendor/generated.rs"), "SCOPE_NEEDLE\n").unwrap();
    std::fs::write(tmp.path().join("secrets/hidden.rs"), "SCOPE_NEEDLE\n").unwrap();
    std::fs::write(tmp.path().join("notes.txt"), "SCOPE_NEEDLE\n").unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "search-globs", "demo", tmp.path()).await;

    let (result, _) = execute_agent_search(
        &runtime,
        "search-globs",
        project,
        SearchRequest {
            pattern: "SCOPE_NEEDLE".to_string(),
            include_globs: Some(vec!["**/*.rs".to_string(), "docs/**/*.md".to_string()]),
            exclude_globs: Some(vec!["vendor/**".to_string()]),
            limit: Some(10),
            ..raw_search_request()
        },
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    // Result order is not deterministic (the search stops as soon as the
    // budget is met rather than sorting the whole repository), so compare as a
    // set.
    let mut paths = result.output["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["path"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    paths.sort();
    assert_eq!(
        paths,
        vec!["docs/guide.md".to_string(), "src/lib.rs".to_string()]
    );
}

#[tokio::test]
async fn search_project_text_files_with_matches_is_unique_stable_and_bounded() {
    // files_with_matches is ripgrep-only; without host rg this is a capability
    // error, not a product regression (see
    // advanced_search_without_rg_returns_structured_capability_error).
    if !host_ripgrep_available() {
        eprintln!("skipping real-ripgrep integration test: rg is unavailable");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("b.rs"), "FILE_NEEDLE\nFILE_NEEDLE\n").unwrap();
    std::fs::write(tmp.path().join("a.rs"), "FILE_NEEDLE\n").unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "search-files", "demo", tmp.path()).await;

    let (result, _) = execute_agent_search(
        &runtime,
        "search-files",
        project,
        SearchRequest {
            pattern: "FILE_NEEDLE".to_string(),
            limit: Some(1),
            result_mode: Some(SearchResultMode::FilesWithMatches),
            ..raw_search_request()
        },
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["result_mode"], "files_with_matches");
    // Either file may be the one reported when limit=1 stops the scan early.
    let files = result.output["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 1);
    assert!(matches!(files[0], "a.rs" | "b.rs"));
    assert_eq!(result.output["returned_file_count"], 1);
    assert_eq!(result.output["truncated"], true);
    assert_eq!(result.output["truncation_reason"], "limit");
}

#[tokio::test]
async fn search_project_text_count_distinguishes_complete_and_truncated_totals() {
    // count result mode is ripgrep-only; without host rg this is a capability
    // error, not a product regression (see
    // advanced_search_without_rg_returns_structured_capability_error).
    if !host_ripgrep_available() {
        eprintln!("skipping real-ripgrep integration test: rg is unavailable");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "COUNT_NEEDLE\nCOUNT_NEEDLE\n").unwrap();
    std::fs::write(tmp.path().join("b.rs"), "COUNT_NEEDLE\n").unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "search-count", "demo", tmp.path()).await;

    let (truncated, _) = execute_agent_search(
        &runtime,
        "search-count",
        project.clone(),
        SearchRequest {
            pattern: "COUNT_NEEDLE".to_string(),
            limit: Some(1),
            result_mode: Some(SearchResultMode::Count),
            ..raw_search_request()
        },
    )
    .await;
    assert!(truncated.success, "{:?}", truncated.error);
    // The truncated count run may stop at whichever file rg happens to reach
    // first, so accept either file as the sole returned record.
    let truncated_files = truncated.output["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(truncated_files.len(), 1);
    assert!(matches!(truncated_files[0].as_str(), "a.rs" | "b.rs"));
    assert_eq!(truncated.output["returned_file_count"], 1);
    assert_eq!(truncated.output["count_complete"], false);
    assert_eq!(truncated.output["total_matches"], Value::Null);
    assert_eq!(truncated.output["truncated"], true);
    // The single returned file's count is the match count rg reported for it
    // (a.rs=2 or b.rs=1), never a claimed total.
    let returned_match_count = truncated.output["returned_match_count"].as_u64().unwrap();
    assert!(matches!(returned_match_count, 1 | 2));

    let (complete, _) = execute_agent_search(
        &runtime,
        "search-count",
        project,
        SearchRequest {
            pattern: "COUNT_NEEDLE".to_string(),
            limit: Some(10),
            result_mode: Some(SearchResultMode::Count),
            ..raw_search_request()
        },
    )
    .await;
    assert!(complete.success, "{:?}", complete.error);
    assert_eq!(complete.output["result_mode"], "count");
    assert_eq!(complete.output["total_matches"], 3);
    for omitted in [
        "backend",
        "returned_file_count",
        "returned_match_count",
        "count_complete",
        "truncated",
        "truncation_reason",
    ] {
        assert!(
            complete.output.get(omitted).is_none(),
            "{omitted}: {}",
            complete.output
        );
    }
    // Both files are present regardless of traversal order.
    let mut complete_files = complete.output["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            (
                file["path"].as_str().unwrap().to_string(),
                file["match_count"].as_u64().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    complete_files.sort();
    assert_eq!(
        complete_files,
        vec![("a.rs".to_string(), 2), ("b.rs".to_string(), 1),]
    );
}

#[tokio::test]
async fn search_project_text_reports_effective_clamped_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "TIMEOUT_NEEDLE\n").unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "search-timeout", "demo", tmp.path()).await;

    let (low, low_req) = execute_agent_search(
        &runtime,
        "search-timeout",
        project.clone(),
        SearchRequest {
            pattern: "TIMEOUT_NEEDLE".to_string(),
            timeout_secs: Some(0),
            ..raw_search_request()
        },
    )
    .await;
    assert!(low.success, "{:?}", low.error);
    assert_eq!(low_req.timeout_secs, 1);
    assert_eq!(low.output["effective_timeout_secs"], 1);

    let (high, high_req) = execute_agent_search(
        &runtime,
        "search-timeout",
        project,
        SearchRequest {
            pattern: "TIMEOUT_NEEDLE".to_string(),
            timeout_secs: Some(999),
            ..raw_search_request()
        },
    )
    .await;
    assert!(high.success, "{:?}", high.error);
    assert!((29..=30).contains(&high_req.timeout_secs));
    let high_effective_timeout = high.output["effective_timeout_secs"].as_u64().unwrap();
    assert!((29..=30).contains(&high_effective_timeout));
}

#[tokio::test]
async fn advanced_search_without_rg_returns_structured_capability_error() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(root.join("a.rs"), "needle\n").unwrap();
    let runtime = test_runtime();
    let project = register_runner_project_at_path(&runtime, "search-no-rg", "demo", &root).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    search_call(
                        project,
                        SearchRequest {
                            result_mode: Some(SearchResultMode::Count),
                            ..raw_search_request()
                        },
                    ),
                    Some(&bootstrap),
                )
                .await
        }
    });
    let mut req = wait_for_patch_agent_request(&runtime, "search-no-rg").await;
    req.command = format!(
        "PATH={}; export PATH\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        req.command
    );
    complete_agent_request_by_running_locally(&runtime, "search-no-rg", req).await;
    let result = extract_single_search_batch_result(task.await.unwrap());

    assert!(!result.success);
    assert_search_output_keys_are_declared(&result.output);
    assert_eq!(result.output["error_kind"], "search_project_text_failed");
    assert_eq!(result.output["failure_stage"], "backend_selection");
    assert_eq!(
        result.output["reason_code"],
        "search_backend_feature_unavailable"
    );
    assert_eq!(result.output["detail_code"], "backend_feature_unavailable");
    assert_eq!(result.output["backend"], "grep");
    assert_eq!(result.output["state_changed"], false);
    assert!(result
        .error
        .unwrap()
        .contains("search_backend_feature_unavailable"));
}

#[tokio::test]
async fn search_project_text_no_matches_returns_empty_matches() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("lib.rs"), "pub fn present() {}\n").unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "search-empty", "demo", tmp.path()).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::SearchProjectTexts {
                        project: project,
                        queries: vec![crate::tool_runtime::SearchProjectTextsQuery {
                            pattern: "absent_needle".to_string(),
                            pattern_mode: None,
                            path: None,
                            limit: Some(5),
                            context_before: None,
                            context_after: None,
                            include_globs: None,
                            exclude_globs: None,
                            result_mode: None,
                            timeout_secs: None,
                        }],
                        session_id: None,
                        max_result_bytes: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "search-empty").await;
    assert!((29..=30).contains(&req.timeout_secs));
    complete_agent_request_by_running_locally(&runtime, "search-empty", req).await;
    let result = extract_single_search_batch_result(task.await.unwrap());

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["matches"], json!([]));
}

#[tokio::test]
async fn search_project_text_excludes_sensitive_and_build_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::create_dir_all(tmp.path().join("target")).unwrap();
    std::fs::create_dir_all(tmp.path().join("node_modules/pkg")).unwrap();
    std::fs::create_dir_all(tmp.path().join("secrets")).unwrap();
    std::fs::create_dir_all(tmp.path().join("tokens")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), "KEEP_SEARCH_NEEDLE\n").unwrap();
    std::fs::write(tmp.path().join(".env"), "KEEP_SEARCH_NEEDLE\n").unwrap();
    std::fs::write(tmp.path().join("target/out.txt"), "KEEP_SEARCH_NEEDLE\n").unwrap();
    std::fs::write(
        tmp.path().join("node_modules/pkg/index.js"),
        "KEEP_SEARCH_NEEDLE\n",
    )
    .unwrap();
    std::fs::write(tmp.path().join("secrets/key.txt"), "KEEP_SEARCH_NEEDLE\n").unwrap();
    std::fs::write(tmp.path().join("tokens/api.txt"), "KEEP_SEARCH_NEEDLE\n").unwrap();
    std::fs::write(tmp.path().join("id.key"), "KEEP_SEARCH_NEEDLE\n").unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "search-excludes", "demo", tmp.path()).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::SearchProjectTexts {
                        project: project,
                        queries: vec![crate::tool_runtime::SearchProjectTextsQuery {
                            pattern: "KEEP_SEARCH_NEEDLE".to_string(),
                            pattern_mode: None,
                            path: None,
                            limit: Some(10),
                            context_before: None,
                            context_after: None,
                            include_globs: None,
                            exclude_globs: None,
                            result_mode: None,
                            timeout_secs: None,
                        }],
                        session_id: None,
                        max_result_bytes: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "search-excludes").await;
    complete_agent_request_by_running_locally(&runtime, "search-excludes", req).await;
    let result = extract_single_search_batch_result(task.await.unwrap());

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["matches"].as_array().unwrap().len(), 1);
    assert_eq!(result.output["matches"][0]["path"], "src/lib.rs");
}

#[tokio::test]
async fn project_overview_routes_to_owning_agent_and_returns_structured_metadata() {
    let temp = tempfile::tempdir().unwrap();
    for path in [
        "AGENTS.md",
        "README.md",
        "Cargo.toml",
        "src/lib.rs",
        "target/debug/output",
        ".env",
    ] {
        let path = temp.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "private fixture content").unwrap();
    }
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "overview-agent", "demo", temp.path()).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::ProjectOverview {
                        project,
                        session_id: None,
                        path: None,
                        max_depth: Some(99),
                        limit: Some(1),
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "overview-agent").await;
    assert_eq!(request.kind, "file_project_overview");
    assert!(
        request.command.is_empty(),
        "project_overview must not use shell"
    );
    let options: Value = serde_json::from_str(request.content.as_deref().unwrap()).unwrap();
    assert_eq!(options["max_depth"], 4);
    assert_eq!(options["limit"], 20);
    complete_project_overview_agent_request_locally(&runtime, "overview-agent", &request).await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["schema_version"], 1);
    assert_eq!(result.output["project"], project);
    assert_eq!(result.output["path"], "");
    assert_eq!(result.output["deterministic"], true);
    assert_eq!(result.output["scan"]["max_depth"], 4);
    assert_eq!(result.output["scan"]["limit"], 20);
    let declared_output = registered_tool_specs()
        .into_iter()
        .find(|spec| spec.name == "project_overview")
        .expect("project_overview spec")
        .output_schema["properties"]["output"]["properties"]
        .as_object()
        .expect("project_overview output schema")
        .clone();
    for key in result.output.as_object().unwrap().keys() {
        assert!(
            declared_output.contains_key(key),
            "runtime project_overview output key {key} is missing from schema"
        );
    }
    let serialized = result.output.to_string();
    assert!(!serialized.contains("private fixture content"));
    assert!(!serialized.contains("target"));
    assert!(!serialized.contains(".env"));
    assert!(!serialized.contains(&temp.path().display().to_string()));
}

#[tokio::test]
async fn project_read_adapters_reject_out_of_project_paths_before_agent_dispatch() {
    let runtime = runtime_with_agent_project("path-boundary");
    register_agent(
        &runtime,
        "path-boundary",
        None,
        RunnerCapabilities {
            file_read: true,
            shell: true,
            ..Default::default()
        },
    )
    .await;
    let bootstrap = auth_context(None, true);
    let project = agent_test_project_id("path-boundary");
    let calls = vec![
        (
            "read_file parent traversal",
            ToolCall::ReadFiles {
                project: project.clone(),
                items: vec![crate::tool_runtime::ReadFilesItem {
                    path: "../outside.txt".to_string(),
                    start_line: None,
                    limit: None,
                }],
                session_id: None,
                with_line_numbers: None,
                max_result_bytes: None,
            },
            None,
        ),
        (
            "read_file nested parent traversal",
            ToolCall::ReadFiles {
                project: project.clone(),
                items: vec![crate::tool_runtime::ReadFilesItem {
                    path: "src/../../outside.txt".to_string(),
                    start_line: None,
                    limit: None,
                }],
                session_id: None,
                with_line_numbers: None,
                max_result_bytes: None,
            },
            None,
        ),
        (
            "read_file absolute path",
            ToolCall::ReadFiles {
                project: project.clone(),
                items: vec![crate::tool_runtime::ReadFilesItem {
                    path: "/etc/passwd".to_string(),
                    start_line: None,
                    limit: None,
                }],
                session_id: None,
                with_line_numbers: None,
                max_result_bytes: None,
            },
            None,
        ),
        (
            "read_file deep parent traversal",
            ToolCall::ReadFiles {
                project: project.clone(),
                items: vec![crate::tool_runtime::ReadFilesItem {
                    path: "sub/../../../etc/passwd".to_string(),
                    start_line: None,
                    limit: None,
                }],
                session_id: None,
                with_line_numbers: None,
                max_result_bytes: None,
            },
            None,
        ),
        (
            "list_project_files absolute path",
            ToolCall::ListProjectFiles {
                project: project.clone(),
                session_id: None,
                path: Some("/etc".to_string()),
                limit: None,
                offset: None,
            },
            None,
        ),
        (
            "list_project_files parent traversal",
            ToolCall::ListProjectFiles {
                project: project.clone(),
                session_id: None,
                path: Some("../outside".to_string()),
                limit: None,
                offset: None,
            },
            None,
        ),
        (
            "project_overview absolute path",
            ToolCall::ProjectOverview {
                project: project.clone(),
                session_id: None,
                path: Some("/etc".to_string()),
                max_depth: None,
                limit: None,
            },
            None,
        ),
        (
            "project_overview parent traversal",
            ToolCall::ProjectOverview {
                project: project.clone(),
                session_id: None,
                path: Some("../outside".to_string()),
                max_depth: None,
                limit: None,
            },
            None,
        ),
        (
            "search_project_text absolute path",
            ToolCall::SearchProjectTexts {
                project: project.clone(),
                queries: vec![crate::tool_runtime::SearchProjectTextsQuery {
                    pattern: "needle".to_string(),
                    pattern_mode: None,
                    path: Some("/etc".to_string()),
                    limit: None,
                    context_before: None,
                    context_after: None,
                    include_globs: None,
                    exclude_globs: None,
                    result_mode: None,
                    timeout_secs: None,
                }],
                session_id: None,
                max_result_bytes: None,
            },
            Some("path"),
        ),
        (
            "search_project_text parent traversal",
            ToolCall::SearchProjectTexts {
                project,
                queries: vec![crate::tool_runtime::SearchProjectTextsQuery {
                    pattern: "needle".to_string(),
                    pattern_mode: None,
                    path: Some("../outside".to_string()),
                    limit: None,
                    context_before: None,
                    context_after: None,
                    include_globs: None,
                    exclude_globs: None,
                    result_mode: None,
                    timeout_secs: None,
                }],
                session_id: None,
                max_result_bytes: None,
            },
            Some("path"),
        ),
    ];

    for (case, call, structured_field) in calls {
        let is_batch = matches!(
            &call,
            ToolCall::ReadFiles { .. } | ToolCall::SearchProjectTexts { .. }
        );
        let result = runtime.dispatch_with_auth(call, Some(&bootstrap)).await;
        let result = if is_batch {
            extract_single_search_batch_result(result)
        } else {
            result
        };
        assert!(!result.success, "{case} escaped the project boundary");
        let error = result.error.as_deref().unwrap_or("");
        assert!(
            error.contains("project-relative")
                || error.contains("parent traversal")
                || error.contains("path"),
            "{case}: {error}"
        );
        if structured_field.is_some() {
            assert_eq!(
                result.output["error_kind"], "search_project_text_failed",
                "{case}"
            );
            assert_eq!(
                result.output["failure_stage"], "request_validation",
                "{case}"
            );
            assert_eq!(result.output["reason_code"], "invalid_path", "{case}");
            assert_eq!(result.output["detail_code"], "invalid_path", "{case}");
        }
        assert!(
            probe_patch_agent_request(&runtime, "path-boundary")
                .await
                .is_none(),
            "{case} must reject before Agent dispatch"
        );
    }
}

#[tokio::test]
async fn search_project_text_requires_shell_capability() {
    let runtime = runtime_with_agent_project("oe");
    let caps = RunnerCapabilities {
        shell: false,
        ..Default::default()
    };
    register_agent(&runtime, "oe", None, caps).await;
    let bootstrap = auth_context(None, true);
    let result = runtime
        .dispatch_with_auth(
            ToolCall::SearchProjectTexts {
                project: agent_test_project_id("oe"),
                queries: vec![crate::tool_runtime::SearchProjectTextsQuery {
                    pattern: "fn".to_string(),
                    pattern_mode: None,
                    path: None,
                    limit: None,
                    context_before: None,
                    context_after: None,
                    include_globs: None,
                    exclude_globs: None,
                    result_mode: None,
                    timeout_secs: None,
                }],
                session_id: None,
                max_result_bytes: None,
            },
            Some(&bootstrap),
        )
        .await;
    assert!(!result.success);
    assert!(
        result.error.unwrap().contains("shell"),
        "search_project_text should require shell capability"
    );
}

#[tokio::test]
async fn search_project_text_context_does_not_enqueue_python_helper() {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("notes.txt"),
        "before\nneedle appears here\nafter\n",
    )
    .unwrap();
    let project =
        register_runner_project_at_path(&runtime, "search-native", "demo", tmp.path()).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::SearchProjectTexts {
                        project: project,
                        queries: vec![crate::tool_runtime::SearchProjectTextsQuery {
                            pattern: "needle".to_string(),
                            pattern_mode: None,
                            path: None,
                            limit: Some(5),
                            context_before: Some(1),
                            context_after: Some(1),
                            include_globs: None,
                            exclude_globs: None,
                            result_mode: None,
                            timeout_secs: None,
                        }],
                        session_id: None,
                        max_result_bytes: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "search-native").await;
    let forbidden = ["python3", "-c"].join(" ");
    assert!(
        !req.command.contains(&forbidden),
        "search context must not enqueue a Python helper: {}",
        req.command
    );
    assert!(req.command.contains("command -v rg"));
    assert!(req.command.contains("rg --with-filename --null"));
    assert!(req.command.contains("grep -rnI --null"));
    complete_agent_request_by_running_locally(&runtime, "search-native", req).await;
    let result = extract_single_search_batch_result(task.await.unwrap());

    assert!(result.success, "{:?}", result.error);
    assert!(matches!(
        result.output.get("backend").and_then(Value::as_str),
        None | Some("grep")
    ));
    assert_eq!(result.output["context_before"], 1);
    assert_eq!(result.output["context_after"], 1);
    let first = &result.output["matches"][0];
    assert_eq!(first["path"], "notes.txt");
    assert_eq!(first["line"], 2);
    assert_eq!(
        first["context_before"][0],
        json!({"line": 1, "text": "before"})
    );
    assert_eq!(
        first["context_after"][0],
        json!({"line": 3, "text": "after"})
    );
}

#[tokio::test]
async fn list_project_files_rejects_non_agent_project_id() {
    // A bare project id (not agent:<client>:<project>) is not resolved by
    // the runtime surface — proving routing goes through the owning agent.
    let runtime = test_runtime();
    let result = runtime
        .dispatch(ToolCall::ListProjectFiles {
            project: "some-local-id".to_string(),
            session_id: None,
            path: None,
            limit: None,
            offset: None,
        })
        .await;
    assert!(!result.success);
    let err = result.error.unwrap();
    assert!(err.contains("agent"), "{err}");
    assert!(!err.contains("projects.toml"), "{err}");
}

#[tokio::test]
async fn search_project_text_rejects_empty_pattern() {
    // Authorization runs before the tool body, so register an agent with
    // shell capability to reach the empty-pattern validation.
    let runtime = runtime_with_agent_project("oe");
    register_agent(
        &runtime,
        "oe",
        None,
        RunnerCapabilities {
            shell: true,
            ..Default::default()
        },
    )
    .await;
    let bootstrap = auth_context(None, true);
    let result = runtime
        .dispatch_with_auth(
            ToolCall::SearchProjectTexts {
                project: agent_test_project_id("oe"),
                queries: vec![crate::tool_runtime::SearchProjectTextsQuery {
                    pattern: "   ".to_string(),
                    pattern_mode: None,
                    path: None,
                    limit: None,
                    context_before: None,
                    context_after: None,
                    include_globs: None,
                    exclude_globs: None,
                    result_mode: None,
                    timeout_secs: None,
                }],
                session_id: None,
                max_result_bytes: None,
            },
            Some(&bootstrap),
        )
        .await;
    let result = extract_single_search_batch_result(result);
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "search_project_text_failed");
    assert_eq!(result.output["failure_stage"], "request_validation");
    assert_eq!(result.output["reason_code"], "invalid_pattern");
    assert_eq!(result.output["detail_code"], "invalid_pattern");
    assert_eq!(result.output["state_changed"], false);
}

#[test]
fn validate_edit_file_path_rejects_unsafe_and_sensitive_paths() {
    // Safe relative paths accepted.
    assert!(validate_edit_file_path("README.md").is_ok());
    assert!(validate_edit_file_path("src/main.rs").is_ok());
    assert!(validate_edit_file_path("a/b/c.txt").is_ok());
    // Empty / NUL / absolute / traversal rejected.
    assert!(validate_edit_file_path("").is_err());
    assert!(validate_edit_file_path("src\0main.rs").is_err());
    assert!(validate_edit_file_path("/etc/passwd").is_err());
    assert!(validate_edit_file_path("../outside").is_err());
    assert!(validate_edit_file_path("src/../../outside").is_err());
    // Sensitive paths hard-rejected.
    for sensitive in [
        "runner.toml",
        "config/runner.toml",
        "runner.toml.bak",
        "agent.toml",
        "config/agent.toml",
        "agent.toml.bak",
        "webcodex.env",
        ".env",
        ".env.local",
        "secrets/projects.d/x",
        "project-registry",
        "projects.d",
        ".git/config",
        "target/debug/bin",
        "node_modules/pkg/index.js",
    ] {
        assert!(
            validate_edit_file_path(sensitive).is_err(),
            "sensitive path should be rejected: {}",
            sensitive
        );
    }
}

#[test]
fn is_sensitive_edit_path_is_component_wise_not_substring() {
    // Component-wise: a filename that merely contains a sensitive token
    // as a substring is NOT rejected.
    assert!(!is_sensitive_edit_path("targeting.md"));
    assert!(!is_sensitive_edit_path("enviroment.rs"));
    assert!(!is_sensitive_edit_path("docs/agent-toml-notes.md"));
    // Exact component matches ARE rejected.
    assert!(is_sensitive_edit_path("target/foo"));
    assert!(is_sensitive_edit_path(".git/HEAD"));
    assert!(is_sensitive_edit_path("node_modules/x"));
    assert!(is_sensitive_edit_path("a/b/.env"));
}

#[test]
fn is_hex_sha256_validates_lowercase_digest() {
    assert!(is_hex_sha256(
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    ));
    assert!(!is_hex_sha256("abc"));
    assert!(!is_hex_sha256(
        "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855"
    ));
    assert!(!is_hex_sha256(
        "z3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    ));
}

#[tokio::test]
async fn write_project_file_rejects_invalid_input_before_agent_dispatch() {
    let runtime = test_runtime();
    // NUL content
    let result = runtime
        .write_project_file(
            "agent:c:p".to_string(),
            "EDIT_PROBE.txt".to_string(),
            "a\0b".to_string(),
            None,
            None,
        )
        .await;
    assert!(!result.success);
    assert!(result.error.unwrap().contains("NUL"));
    // sensitive path
    let result = runtime
        .write_project_file(
            "agent:c:p".to_string(),
            ".env".to_string(),
            "x".to_string(),
            None,
            None,
        )
        .await;
    assert!(!result.success);
    assert!(result.error.unwrap().contains("sensitive"));
    // bad expected_sha256 format
    let result = runtime
        .write_project_file(
            "agent:c:p".to_string(),
            "EDIT_PROBE.txt".to_string(),
            "x".to_string(),
            Some(true),
            Some("not-a-hash".to_string()),
        )
        .await;
    assert!(!result.success);
    assert!(result.error.unwrap().contains("expected_sha256"));
}

#[test]
fn removed_legacy_edit_tools_are_rejected_as_unknown() {
    // The 7 legacy edit tools are no longer known ToolDefinitions, so any
    // dispatch attempt resolves to the standard unknown-tool rejection before
    // any project or file-op work. This replaces the old back-compat dispatch
    // tests for these tools.
    for name in [
        "replace_in_file",
        "replace_exact_block",
        "insert_before_pattern",
        "insert_after_pattern",
        "replace_line_range",
        "insert_at_line",
        "delete_line_range",
    ] {
        let err = ToolCall::from_tool_name(name, json!({})).unwrap_err();
        assert!(err.contains("unknown tool"), "{name}: {err}");
    }
}

#[test]
fn validate_artifact_file_path_rejects_unsafe_and_sensitive_paths() {
    assert!(validate_artifact_file_path("docs/assets/generated.png").is_ok());
    for path in [
        "/absolute/evil.png",
        "\\rooted\\evil.png",
        "C:\\absolute\\evil.png",
        "C:drive-relative\\evil.png",
        "../evil.png",
        "nested\\..\\evil.png",
        ".git/config",
        ".git\\config",
        ".env",
        "secrets/key.pem",
        "secrets\\key.pem",
        "tokens/api.txt",
        "certs\\server.key",
        "config\\agent.toml",
        "target/out.bin",
        "node_modules/pkg/file",
    ] {
        assert!(
            validate_artifact_file_path(path).is_err(),
            "{} should be rejected",
            path
        );
    }
}

#[tokio::test]
async fn read_project_artifact_rejects_sensitive_path_before_resolving_project() {
    let out = test_runtime()
        .read_project_artifact(
            "agent:missing:missing".to_string(),
            ".env".to_string(),
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(!out.success);
    assert!(out.error.unwrap().contains("sensitive artifact path"));
}

#[tokio::test]
async fn read_project_artifact_rejects_invalid_length_before_resolving_project() {
    let out = test_runtime()
        .read_project_artifact(
            "agent:missing:missing".to_string(),
            "docs/assets/file.png".to_string(),
            None,
            None,
            Some(crate::tool_runtime::files::MAX_READ_PROJECT_ARTIFACT_LENGTH + 1),
            None,
        )
        .await;
    assert!(!out.success);
    assert!(out.error.unwrap().contains("length too large"));
}

#[tokio::test]
async fn office_artifact_mime_policy_accepts_matching_save_and_upload_paths() {
    let runtime = test_runtime();
    let missing_project = "agent:missing:missing".to_string();
    let cases = [
        (
            "docs/report.docx",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "docs/report.pptx",
        ),
        (
            "slides/deck.pptx",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "slides/deck.xlsx",
        ),
        (
            "data/book.xlsx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "data/book.docx",
        ),
    ];

    for (path, mime, mismatched_path) in cases {
        let save = runtime
            .save_project_artifact(
                missing_project.clone(),
                path.to_string(),
                "YQ==".to_string(),
                Some(mime.to_string()),
                Some(false),
            )
            .await;
        assert!(!save.success, "{path}");
        assert!(
            !save
                .error
                .as_deref()
                .unwrap()
                .contains("unsupported mime_type")
                && !save
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("requires a matching"),
            "matching Office MIME should pass policy before project resolution: {:?}",
            save.error
        );

        let upload = runtime
            .artifact_upload_begin(
                missing_project.clone(),
                path.to_string(),
                Some(1),
                None,
                Some(mime.to_string()),
                Some(false),
            )
            .await;
        assert!(!upload.success, "{path}");
        assert!(
            !upload
                .error
                .as_deref()
                .unwrap()
                .contains("unsupported mime_type")
                && !upload
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("requires a matching"),
            "matching Office upload MIME should pass policy before project resolution: {:?}",
            upload.error
        );

        let octet = runtime
            .artifact_upload_begin(
                missing_project.clone(),
                path.to_string(),
                Some(1),
                None,
                Some("application/octet-stream".to_string()),
                Some(false),
            )
            .await;
        assert!(!octet.success, "{path}");
        assert!(
            !octet
                .error
                .as_deref()
                .unwrap()
                .contains("only allowed for safe artifact extensions"),
            "Office extensions should be safe octet-stream artifact paths: {:?}",
            octet.error
        );

        let mismatched = runtime
            .save_project_artifact(
                missing_project.clone(),
                mismatched_path.to_string(),
                "YQ==".to_string(),
                Some(mime.to_string()),
                Some(false),
            )
            .await;
        assert!(!mismatched.success);
        assert!(
            mismatched
                .error
                .as_deref()
                .unwrap()
                .contains("requires a matching"),
            "{:?}",
            mismatched.error
        );
    }

    let unsupported = runtime
        .save_project_artifact(
            missing_project,
            "docs/report.docx".to_string(),
            "YQ==".to_string(),
            Some("application/msword".to_string()),
            Some(false),
        )
        .await;
    assert!(!unsupported.success);
    assert!(
        unsupported
            .error
            .as_deref()
            .unwrap()
            .contains("unsupported mime_type"),
        "{:?}",
        unsupported.error
    );
}

#[tokio::test]
async fn common_media_artifact_mime_policy_accepts_save_upload_and_octet_paths() {
    let runtime = test_runtime();
    let missing_project = "agent:missing:missing".to_string();
    for (path, mime) in [
        ("media/sample.mp3", "audio/mpeg"),
        ("media/sample.mp4", "video/mp4"),
    ] {
        let save = runtime
            .save_project_artifact(
                missing_project.clone(),
                path.to_string(),
                "YQ==".to_string(),
                Some(mime.to_string()),
                Some(false),
            )
            .await;
        assert!(!save.success, "{path}");
        assert!(
            !save
                .error
                .as_deref()
                .unwrap()
                .contains("unsupported mime_type"),
            "media MIME should pass policy before project resolution: {:?}",
            save.error
        );

        let upload = runtime
            .artifact_upload_begin(
                missing_project.clone(),
                path.to_string(),
                Some(1),
                None,
                Some(mime.to_string()),
                Some(false),
            )
            .await;
        assert!(!upload.success, "{path}");
        assert!(
            !upload
                .error
                .as_deref()
                .unwrap()
                .contains("unsupported mime_type"),
            "media upload MIME should pass policy before project resolution: {:?}",
            upload.error
        );

        let octet = runtime
            .artifact_upload_begin(
                missing_project.clone(),
                path.to_string(),
                Some(1),
                None,
                Some("application/octet-stream".to_string()),
                Some(false),
            )
            .await;
        assert!(!octet.success, "{path}");
        assert!(
            !octet
                .error
                .as_deref()
                .unwrap()
                .contains("only allowed for safe artifact extensions"),
            "media extension should be safe for host octet-stream fallback: {:?}",
            octet.error
        );
    }
}

#[tokio::test]
async fn artifact_upload_begin_rejects_invalid_inputs_before_resolving_project() {
    let runtime = test_runtime();
    let missing_project = "agent:missing:missing".to_string();
    let cases = [
        (
            ".env",
            Some(1),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Some("text/plain"),
            "sensitive artifact path",
        ),
        (
            "artifacts/imports/bad-hash.txt",
            Some(1),
            Some("not-a-sha"),
            Some("text/plain"),
            "expected_sha256 must be a lowercase 64-char hex sha256 digest",
        ),
        (
            "artifacts/imports/too-large.txt",
            Some(MAX_PROJECT_ARTIFACT_UPLOAD_BYTES + 1),
            None,
            Some("text/plain"),
            "expected_bytes too large",
        ),
        (
            "artifacts/imports/raw.bin",
            Some(1),
            None,
            Some("application/octet-stream"),
            "artifacts/smoke/<name>.artifact",
        ),
    ];

    for (path, expected_bytes, expected_sha256, mime_type, expected_error) in cases {
        let out = runtime
            .artifact_upload_begin(
                missing_project.clone(),
                path.to_string(),
                expected_bytes,
                expected_sha256.map(str::to_string),
                mime_type.map(str::to_string),
                Some(false),
            )
            .await;
        assert!(!out.success, "{path}");
        assert!(
            out.error.as_deref().unwrap().contains(expected_error),
            "{path}: {:?}",
            out.error
        );
    }
}

#[tokio::test]
async fn artifact_upload_chunk_rejects_invalid_inputs_before_resolving_project() {
    let runtime = test_runtime();
    let missing_project = "agent:missing:missing".to_string();
    let path = "artifacts/imports/chunk.txt".to_string();

    let invalid_id = runtime
        .artifact_upload_chunk(
            missing_project.clone(),
            path.clone(),
            "bad-upload-id".to_string(),
            0,
            "YQ==".to_string(),
        )
        .await;
    assert!(!invalid_id.success);
    assert!(invalid_id.error.unwrap().contains("upload_id must start"));

    let invalid_base64 = runtime
        .artifact_upload_chunk(
            missing_project.clone(),
            path.clone(),
            "wc_upload_test_1".to_string(),
            0,
            "not valid base64!".to_string(),
        )
        .await;
    assert!(!invalid_base64.success);
    assert!(invalid_base64.error.unwrap().contains("invalid base64"));

    let empty = runtime
        .artifact_upload_chunk(
            missing_project.clone(),
            path.clone(),
            "wc_upload_test_1".to_string(),
            0,
            "".to_string(),
        )
        .await;
    assert!(!empty.success);
    assert!(empty
        .error
        .unwrap()
        .contains("decoded chunk must contain at least 1 byte"));

    let oversized = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        vec![b'x'; MAX_PROJECT_ARTIFACT_UPLOAD_CHUNK_BYTES + 1],
    );
    let oversized = runtime
        .artifact_upload_chunk(
            missing_project,
            path,
            "wc_upload_test_1".to_string(),
            0,
            oversized,
        )
        .await;
    assert!(!oversized.success);
    assert!(oversized.error.unwrap().contains("decoded chunk too large"));
}

#[tokio::test]
async fn artifact_upload_finish_and_abort_reject_invalid_upload_id_before_resolving_project() {
    let runtime = test_runtime();
    let missing_project = "agent:missing:missing".to_string();
    let path = "artifacts/imports/file.txt".to_string();

    let finish = runtime
        .artifact_upload_finish(missing_project.clone(), path.clone(), "bad".to_string())
        .await;
    assert!(!finish.success);
    assert!(finish.error.unwrap().contains("upload_id must start"));

    let abort = runtime
        .artifact_upload_abort(missing_project, path, "bad".to_string())
        .await;
    assert!(!abort.success);
    assert!(abort.error.unwrap().contains("upload_id must start"));
}

#[tokio::test]
async fn read_file_routes_safe_and_bulk_skipped_explicit_paths_to_agent() {
    for (client_id, path, content) in [
        ("relative-read", "src/main.rs", "fn main() {}\n"),
        (
            "bulk-explicit-read",
            "node_modules/foo/package.json",
            "{}\n",
        ),
    ] {
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
        let task = tokio::spawn({
            let runtime = runtime.clone();
            let path = path.to_string();
            async move { runtime.read_file(project, path, None, None, None).await }
        });

        let request = wait_for_patch_agent_request(&runtime, client_id).await;
        assert_eq!(request.kind, "file_read", "{path}");
        assert_eq!(request.path.as_deref(), Some(path), "{path}");
        complete_agent_ranged_file_read_request(&runtime, client_id, &request, content).await;
        let result = task.await.unwrap();
        assert!(result.success, "{path}: {:?}", result.error);
    }
}

#[tokio::test]
async fn read_file_refuses_secret_paths_before_reaching_agent() {
    // Search excluded credentials, artifacts and edits rejected them, but
    // read_file returned them verbatim. Case variants must be refused too:
    // the old search predicate was case-sensitive.
    let runtime = runtime_with_agent_project("secret-read");
    let caps = RunnerCapabilities {
        file_read: true,
        ..Default::default()
    };
    register_agent(&runtime, "secret-read", None, caps).await;
    let project = agent_test_project_id("secret-read");

    for path in [
        ".git/config",
        ".git/HEAD",
        ".env",
        ".env.production",
        "app/.env.local",
        ".ENV",
        "certs/server.pem",
        "certs/server.key",
        "certs/Server.PEM",
        "runner.toml",
        "agent.toml",
        "secrets/token",
        "tokens/agent",
        "project-registry/demo.toml",
        "projects.d/demo.toml",
    ] {
        let result = runtime
            .read_file(project.clone(), path.to_string(), None, None, None)
            .await;
        assert!(!result.success, "read_file returned secret path {path:?}");
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("sensitive")),
            "unexpected error for {path:?}: {:?}",
            result.error
        );
    }

    assert!(
        probe_patch_agent_request(&runtime, "secret-read")
            .await
            .is_none(),
        "a refused secret path still reached the agent"
    );
}

/// `--no-ignore` used to be passed to ripgrep, so a search walked straight
/// through `.gitignore` and returned build output, virtualenvs, and vendored
/// trees. Dropping it means the project's own ignore rules apply.
#[cfg(unix)]
#[test]
fn search_project_text_respects_gitignore() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(root.join("build")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
    std::fs::write(root.join("src/kept.rs"), "needle here\n").unwrap();
    std::fs::write(root.join("build/generated.rs"), "needle here\n").unwrap();

    let options = SearchOptions::normalize(raw_search_request()).unwrap();
    let command = search_project_text_command(&options);
    let (exit_code, stdout, stderr, _) = run_command_sync(&command, &root, 10);
    if exit_code == 127 {
        return; // no ripgrep on this host; the grep fallback is covered below
    }
    assert!(exit_code == 0 || exit_code == 1, "stderr={stderr}");
    assert!(stdout.contains("kept.rs"), "stdout={stdout}");
    assert!(
        !stdout.contains("generated.rs"),
        "gitignored build output was searched: {stdout}"
    );
}

/// Honouring `.gitignore` must not become the only protection: a repository
/// that commits its `.env` — or simply does not ignore it — still has to be
/// excluded from search results.
#[cfg(unix)]
#[test]
fn search_project_text_still_excludes_sensitive_paths_not_in_gitignore() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(root.join("src")).unwrap();
    // Deliberately no .gitignore: nothing here is ignored by the project.
    std::fs::write(root.join("src/kept.rs"), "needle here\n").unwrap();
    std::fs::write(root.join(".env"), "needle here\n").unwrap();

    let options = SearchOptions::normalize(raw_search_request()).unwrap();
    let command = search_project_text_command(&options);
    let (exit_code, stdout, stderr, _) = run_command_sync(&command, &root, 10);
    if exit_code == 127 {
        return;
    }
    assert!(exit_code == 0 || exit_code == 1, "stderr={stderr}");
    assert!(stdout.contains("kept.rs"), "stdout={stdout}");
    let result = search_project_text_output("demo", &options, &stdout, Some(exit_code), &stderr);
    let serialized = serde_json::to_string(&result.output).unwrap();
    assert!(
        !serialized.contains(".env"),
        "a sensitive path reached the search result: {serialized}"
    );
}

/// The grep fallback cannot read `.gitignore`, so it keeps an explicit exclude
/// list. That list is defence in depth, not a substitute — it must still be
/// there.
#[cfg(unix)]
#[test]
fn grep_fallback_keeps_defense_in_depth_excludes() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(root.join("node_modules")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/kept.rs"), "needle here\n").unwrap();
    std::fs::write(root.join("node_modules/vendored.js"), "needle here\n").unwrap();
    // A PATH holding the real grep but no ripgrep, so the command genuinely
    // takes the fallback branch.
    for tool in ["grep", "head", "sh"] {
        if let Ok(found) = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {tool}"))
            .output()
        {
            let path = String::from_utf8_lossy(&found.stdout).trim().to_string();
            if !path.is_empty() {
                let _ = std::os::unix::fs::symlink(&path, bin.join(tool));
            }
        }
    }

    let options = SearchOptions::normalize(raw_search_request()).unwrap();
    let command = format!(
        "PATH={}; export PATH\n{}",
        shell_escape_simple(&bin.to_string_lossy()),
        search_project_text_command(&options)
    );
    let (exit_code, stdout, stderr, _) = run_command_sync(&command, &root, 10);
    assert!(
        exit_code == 0 || exit_code == 1,
        "exit={exit_code} stderr={stderr} stdout={stdout}"
    );
    assert!(stdout.contains("kept.rs"), "stdout={stdout}");
    assert!(
        !stdout.contains("vendored.js"),
        "grep fallback lost its node_modules exclude: {stdout}"
    );
}
