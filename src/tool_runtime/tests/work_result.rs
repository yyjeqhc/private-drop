use super::super::work_result::{build_work_result_projection, MAX_WORK_RESULT_FILES};
use super::super::*;
use super::support::*;
use serde_json::{json, Value};

fn counts(conflicted: u64) -> Value {
    json!({
        "modified": 2,
        "added": 1,
        "deleted": 0,
        "renamed": 0,
        "copied": 0,
        "untracked": 1,
        "conflicted": conflicted,
        "staged": 0,
        "unstaged": 3
    })
}

fn validation(status: &str, latest: &str, successes: u64, failures: u64) -> Value {
    json!({
        "status": status,
        "latest_status": latest,
        "successes": successes,
        "failures": failures
    })
}

fn current_validation(status: &str, unresolved: u64, gaps: u64) -> Value {
    json!({
        "status": status,
        "unresolved_failure_count": unresolved,
        "evidence_gap_event_count": gaps
    })
}

fn review(total: u64) -> Value {
    json!({
        "available": true,
        "total": total,
        "read_only_inspection_count": total,
        "search_count": 0,
        "diff_review_count": total,
        "workspace_review_count": total,
        "hygiene_review_count": 0,
        "tools": ["show_changes", "git_review_summary"]
    })
}

#[test]
fn work_result_projection_is_sparse_bounded_and_honest() {
    let mut files = Vec::new();
    for index in 0..10 {
        files.push(json!({
            "path": format!("src/file_{index}.rs"),
            "status": if index == 0 { "added" } else { "modified" },
            "kind": "tracked",
            "staged": false,
            "unstaged": true,
            "additions": index + 1,
            "deletions": index
        }));
    }
    files.push(json!({"path": "src/binary.bin", "status": "modified", "kind": "tracked"}));
    files.push(json!({"path": "../escape", "status": "modified", "additions": 9, "deletions": 9}));
    let workspace = json!({
        "git_available": true,
        "clean": false,
        "branch": "feature/work-result",
        "head": {"commit": "a".repeat(40), "short": "aaaaaaaa"},
        "counts": counts(1),
        "files_total": files.len(),
        "files_truncated": false,
        "files": files
    });
    let projected = build_work_result_projection(
        "agent:special:demo",
        &format!("wc_sess_{}", "1".repeat(32)),
        true,
        &workspace,
        &validation("mixed", "passed", 8, 1),
        &current_validation("failed", 1, 2),
        &review(2),
    );
    assert_eq!(
        projected["workspace"]["files"].as_array().unwrap().len(),
        MAX_WORK_RESULT_FILES
    );
    assert_eq!(projected["workspace"]["truncated"], true);
    assert_eq!(projected["workspace"]["line_stats_partial"], true);
    assert_eq!(projected["workspace"]["counts"]["conflicted"], 1);
    assert_eq!(projected["validation"]["status"], "mixed");
    assert_eq!(projected["validation"]["current_status"], "failed");
    assert_eq!(projected["validation"]["unresolved_failures"], 1);
    assert_eq!(projected["validation"]["evidence_gaps"], 2);
    assert_eq!(projected["review"]["total"], 2);
    assert!(projected["review"].get("passed").is_none());
    let serialized = projected.to_string();
    for private in ["stdout", "stderr", "job_id", "continuation", "message_body"] {
        assert!(!serialized.contains(private));
    }
}

#[test]
fn work_result_projection_handles_clean_non_git_and_unknown_validation_without_invention() {
    let clean = build_work_result_projection(
        "agent:special:demo",
        &format!("wc_sess_{}", "2".repeat(32)),
        true,
        &json!({
            "git_available": true,
            "clean": true,
            "counts": counts(0),
            "files_total": 0,
            "files": []
        }),
        &validation("not_run", "not_run", 0, 0),
        &current_validation("not_run", 0, 0),
        &json!({"available": true, "total": 0}),
    );
    assert_eq!(clean["workspace"]["clean"], true);
    assert_eq!(clean["workspace"]["additions"], 0);
    assert_eq!(clean["workspace"]["deletions"], 0);
    assert_eq!(clean["validation"]["current_status"], "not_run");
    assert_eq!(clean["review"]["total"], 0);

    let unavailable = build_work_result_projection(
        "agent:special:demo",
        &format!("wc_sess_{}", "3".repeat(32)),
        true,
        &json!({
            "git_available": false,
            "non_git_project": true,
            "counts": {},
            "files_total": 0,
            "files": []
        }),
        &json!({"status": "future_value", "latest_status": "future_value"}),
        &json!({"status": "future_value"}),
        &Value::Null,
    );
    assert_eq!(unavailable["workspace"]["git_available"], false);
    assert_eq!(unavailable["workspace"]["reason_code"], "non_git_project");
    assert_eq!(unavailable["validation"]["status"], "unknown");
    assert_eq!(unavailable["validation"]["current_status"], "unknown");
    assert_eq!(unavailable["review"]["available"], false);
}

async fn poll_once(
    runtime: &ToolRuntime,
    client_id: &str,
    project: &str,
    session_id: &str,
    auth: &crate::auth::AuthContext,
) -> ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.to_string();
        let session_id = session_id.to_string();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::WorkResultState {
                        project,
                        session_id,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(runtime, client_id).await;
    assert_eq!(request.kind, "run_internal_posix_script");
    complete_agent_request_by_running_locally(runtime, client_id, request).await;
    task.await.unwrap()
}

#[tokio::test]
async fn work_result_state_reauthorizes_exact_identity_and_polling_does_not_record_target_session()
{
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "work-result", "demo", tmp.path()).await;
    let auth = auth_context(None, true);
    let session = runtime.sessions.start_session(
        Some(project.clone()),
        Some("Work Result polling".to_string()),
    );
    let before = runtime.sessions.summary(&session.session_id, None).unwrap();

    let first = poll_once(
        &runtime,
        "work-result",
        &project,
        &session.session_id,
        &auth,
    )
    .await;
    assert!(first.success, "{:?}", first.error);
    let first_version = first.output["work_result"]["state_version"]
        .as_str()
        .unwrap()
        .to_string();
    let second = poll_once(
        &runtime,
        "work-result",
        &project,
        &session.session_id,
        &auth,
    )
    .await;
    assert!(second.success, "{:?}", second.error);
    assert_eq!(second.output["work_result"]["state_version"], first_version);

    let alias_state = runtime
        .work_result_state("demo".to_string(), session.session_id.clone(), Some(&auth))
        .await;
    assert!(!alias_state.success);
    assert_eq!(
        alias_state.output["error_kind"],
        "work_result_project_not_exact"
    );
    let alias_present = runtime
        .present_work_result("demo".to_string(), session.session_id.clone(), Some(&auth))
        .await;
    assert!(!alias_present.success);
    assert_eq!(
        alias_present.output["error_kind"],
        "work_result_project_not_exact"
    );
    assert!(
        probe_patch_agent_request(&runtime, "work-result")
            .await
            .is_none(),
        "a non-canonical project alias must fail before workspace observation"
    );

    let after = runtime.sessions.summary(&session.session_id, None).unwrap();
    assert_eq!(after.events_total, before.events_total);
    assert_eq!(after.events.len(), before.events.len());
    assert_eq!(after.updated_at, before.updated_at);
    assert_eq!(
        ToolCall::WorkResultState {
            project: project.clone(),
            session_id: session.session_id.clone()
        }
        .session_id(),
        None
    );
    assert_eq!(
        ToolCall::PresentWorkResult {
            project: project.clone(),
            session_id: session.session_id.clone()
        }
        .session_id(),
        Some(session.session_id.as_str())
    );

    let other_tmp = tempfile::tempdir().unwrap();
    init_git_repo(other_tmp.path());
    commit_file(other_tmp.path(), "README.md", "other\n", "other");
    let other_project =
        register_runner_project_at_path(&runtime, "work-result-other", "other", other_tmp.path())
            .await;
    let mismatch = runtime
        .work_result_state(
            other_project.clone(),
            session.session_id.clone(),
            Some(&auth),
        )
        .await;
    assert!(!mismatch.success);
    assert_eq!(mismatch.output["error_kind"], "session_project_mismatch");
    let present_mismatch = runtime
        .present_work_result(other_project, session.session_id.clone(), Some(&auth))
        .await;
    assert!(!present_mismatch.success);
    assert_eq!(
        present_mismatch.output["error_kind"],
        "session_project_mismatch"
    );
}

#[tokio::test]
async fn work_result_state_fails_closed_for_foreign_session_authority() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "work-result-auth", "demo", tmp.path()).await;
    let bob = shared_key_auth_context("work-result-bob");
    let alice = shared_key_auth_context("work-result-alice");
    let fingerprint = workflow_session_authority_fingerprint(Some(&bob)).unwrap();
    let session = runtime
        .sessions
        .start_session_with_options(
            SessionCreateOptions::new(
                Some(project.clone()),
                Some("private Work Result".to_string()),
                SessionMode::Normal,
                SessionGuards::default(),
            )
            .with_owner_authority_fingerprint(Some(fingerprint)),
        )
        .unwrap();
    let denied = runtime
        .work_result_state(project.clone(), session.session_id.clone(), Some(&alice))
        .await;
    assert!(!denied.success);
    assert!(denied.output.get("work_result").is_none());
    assert!(denied
        .error
        .as_deref()
        .is_some_and(|error| !error.is_empty()));
    let present_denied = runtime
        .present_work_result(project, session.session_id, Some(&alice))
        .await;
    assert!(!present_denied.success);
    assert!(present_denied.output.get("work_result").is_none());
    assert!(present_denied
        .error
        .as_deref()
        .is_some_and(|error| !error.is_empty()));
    assert!(
        probe_patch_agent_request(&runtime, "work-result-auth")
            .await
            .is_none(),
        "an inaccessible Session must fail before any workspace polling request"
    );
}

#[test]
fn work_result_tool_contract_requires_exact_project_and_session() {
    for name in ["present_work_result", "work_result_state"] {
        assert!(ToolCall::from_tool_name(name, json!({"project": "agent:x:y"})).is_err());
        assert!(ToolCall::from_tool_name(
            name,
            json!({"session_id": format!("wc_sess_{}", "1".repeat(32))})
        )
        .is_err());
        let call = ToolCall::from_tool_name(
            name,
            json!({
                "project": "agent:x:y",
                "session_id": format!("wc_sess_{}", "1".repeat(32))
            }),
        )
        .unwrap();
        assert_eq!(call.tool_name(), name);
    }
}
