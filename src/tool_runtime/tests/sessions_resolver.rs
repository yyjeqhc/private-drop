//! Project resolver tests for tool_runtime.

use super::super::*;
use super::support::*;
use crate::runner_protocol::RunnerResultRequest;

#[tokio::test]
async fn project_resolver_resolves_full_id() {
    let runtime = runtime_with_resolver_projects().await;
    let resolved = runtime
        .resolve_project_input("agent:workstation:my-repo")
        .await
        .unwrap();
    assert_eq!(resolved.resolved_id, "agent:workstation:my-repo");
    assert_eq!(resolved.config.client_id, "workstation");
    assert_eq!(resolved.config.path, "/root/git/workstation-my-repo");
}

#[tokio::test]
async fn project_resolver_resolves_client_project_shorthand() {
    let runtime = runtime_with_resolver_projects().await;
    let resolved = runtime
        .resolve_project_input("workstation:my-repo")
        .await
        .unwrap();
    assert_eq!(resolved.resolved_id, "agent:workstation:my-repo");
}

#[tokio::test]
async fn project_resolver_resolves_unique_short_id() {
    let runtime = runtime_with_resolver_projects().await;
    let resolved = runtime.resolve_project_input("other-repo").await.unwrap();
    assert_eq!(resolved.resolved_id, "agent:workstation:other-repo");
}

#[tokio::test]
async fn project_resolver_ambiguous_short_id_returns_candidates() {
    let runtime = runtime_with_resolver_projects().await;
    let err = runtime.resolve_project_input("my-repo").await.unwrap_err();
    assert_eq!(err.kind, ProjectResolverErrorKind::AmbiguousProject);
    assert_eq!(err.project, "my-repo");
    let ids: Vec<String> = err
        .candidates
        .iter()
        .map(|candidate| candidate.id.clone())
        .collect();
    assert_eq!(
        ids,
        vec![
            "agent:laptop:my-repo".to_string(),
            "agent:workstation:my-repo".to_string(),
        ]
    );
}

#[tokio::test]
async fn project_resolver_unknown_id_returns_candidates() {
    let runtime = runtime_with_resolver_projects().await;
    let err = runtime
        .resolve_project_input("missing-repo")
        .await
        .unwrap_err();
    assert_eq!(err.kind, ProjectResolverErrorKind::UnknownProject);
    assert_eq!(err.project, "missing-repo");
    assert!(err.candidates.len() >= 3);
    assert!(err
        .candidates
        .iter()
        .any(|candidate| candidate.id == "agent:workstation:other-repo"));
}

#[tokio::test]
async fn read_file_accepts_unique_short_id() {
    let runtime = runtime_with_resolver_projects().await;
    let bootstrap = auth_context(None, true);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFile {
                        project: "other-repo".to_string(),
                        path: "README.md".to_string(),
                        session_id: None,
                        start_line: None,
                        limit: None,
                        with_line_numbers: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_runner_request_for_client(&runtime, "workstation").await;
    assert_eq!(req.cwd.as_deref(), Some("/root/git/workstation-other-repo"));
    runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: "workstation".to_string(),
            runner_instance_id: "inst-workstation".to_string(),
            request_id: req.request_id,
            exit_code: Some(0),
            stdout: Some(canonical_agent_file_read_output("hello\n", 1)),
            stderr: None,
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap();
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
}

#[tokio::test]
async fn read_file_short_id_continuation_binds_resolved_project_across_registry_churn() {
    let runtime = runtime_with_resolver_projects().await;
    let auth = auth_context(None, true);
    let first = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFile {
                        project: "other-repo".to_string(),
                        path: "README.md".to_string(),
                        session_id: None,
                        start_line: Some(1),
                        limit: Some(1),
                        with_line_numbers: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let req = wait_for_runner_request_for_client(&runtime, "workstation").await;
    assert_eq!(req.cwd.as_deref(), Some("/root/git/workstation-other-repo"));
    runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: "workstation".to_string(),
            runner_instance_id: "inst-workstation".to_string(),
            request_id: req.request_id,
            exit_code: Some(0),
            stdout: Some(canonical_agent_file_read_range("one\ntwo", 1, 1)),
            stderr: None,
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap();
    let first = first.await.unwrap();
    assert!(first.success, "{:?}", first.error);
    let suggested = &first.output["continuation"]["suggested_call"];
    assert_eq!(
        suggested["arguments"]["project"],
        "agent:workstation:other-repo"
    );
    assert!(suggested["arguments"].get("session_id").is_none());
    let next_call = ToolCall::from_tool_name(
        suggested["tool"].as_str().unwrap(),
        suggested["arguments"].clone(),
    )
    .expect("short-id continuation suggested_call must parse");

    // Make the original shorthand ambiguous after the first read. The already
    // constructed recovery call must remain bound to workstation.
    register_agent_projects(
        &runtime,
        "laptop",
        None,
        crate::runner_protocol::RunnerCapabilities {
            file_read: true,
            git: true,
            shell: true,
            internal_posix_script: true,
            ..Default::default()
        },
        vec![
            named_registered_project(
                "laptop",
                "my-repo",
                "My Repo",
                "/root/git/laptop-my-repo",
                190,
            ),
            named_registered_project(
                "laptop",
                "other-repo",
                "Other Repo",
                "/root/git/laptop-other-repo",
                220,
            ),
        ],
    )
    .await;
    let ambiguous = runtime
        .resolve_project_input("other-repo")
        .await
        .unwrap_err();
    assert_eq!(ambiguous.kind, ProjectResolverErrorKind::AmbiguousProject);

    let second = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        async move { runtime.dispatch_with_auth(next_call, Some(&auth)).await }
    });
    let req = wait_for_runner_request_for_client(&runtime, "workstation").await;
    assert_eq!(req.cwd.as_deref(), Some("/root/git/workstation-other-repo"));
    assert_eq!(req.start_line, Some(2));
    runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: "workstation".to_string(),
            runner_instance_id: "inst-workstation".to_string(),
            request_id: req.request_id,
            exit_code: Some(0),
            stdout: Some(canonical_agent_file_read_range("one\ntwo", 2, 1)),
            stderr: None,
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap();
    let second = second.await.unwrap();
    assert!(second.success, "{:?}", second.error);
    assert_eq!(second.output["text"], "two");
}

#[tokio::test]
async fn read_files_short_id_item_continuation_uses_resolved_project_id() {
    let runtime = runtime_with_resolver_projects().await;
    let auth = auth_context(None, true);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFiles {
                        project: "other-repo".to_string(),
                        items: vec![ReadFilesItem {
                            path: "README.md".to_string(),
                            start_line: Some(1),
                            limit: Some(1),
                        }],
                        session_id: None,
                        with_line_numbers: None,
                        max_result_bytes: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let req = wait_for_runner_request_for_client(&runtime, "workstation").await;
    runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: "workstation".to_string(),
            runner_instance_id: "inst-workstation".to_string(),
            request_id: req.request_id,
            exit_code: Some(0),
            stdout: Some(canonical_agent_file_read_range("one\ntwo", 1, 1)),
            stderr: None,
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap();
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        result.output["items"][0]["continuation"]["suggested_call"]["arguments"]["project"],
        "agent:workstation:other-repo"
    );
    assert!(
        result.output["items"][0]["continuation"]["suggested_call"]["arguments"]
            .get("session_id")
            .is_none()
    );
}

#[tokio::test]
async fn git_status_accepts_unique_short_id() {
    let runtime = runtime_with_resolver_projects().await;
    let bootstrap = auth_context(None, true);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::GitStatus {
                        project: "other-repo".to_string(),
                        session_id: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_runner_request_for_client(&runtime, "workstation").await;
    assert_eq!(req.cwd.as_deref(), Some("/root/git/workstation-other-repo"));
    runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: "workstation".to_string(),
            runner_instance_id: "inst-workstation".to_string(),
            request_id: req.request_id,
            exit_code: Some(0),
            stdout: Some(String::new()),
            stderr: Some(String::new()),
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap();
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
}

#[tokio::test]
async fn ambiguous_short_id_returns_candidates_for_project_tools() {
    let runtime = runtime_with_resolver_projects().await;
    let bootstrap = auth_context(None, true);
    let result = runtime
        .dispatch_with_auth(
            ToolCall::ReadFile {
                project: "my-repo".to_string(),
                path: "README.md".to_string(),
                session_id: None,
                start_line: None,
                limit: None,
                with_line_numbers: None,
            },
            Some(&bootstrap),
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "ambiguous_project");
    assert_eq!(result.output["project"], "my-repo");
    assert_eq!(result.output["candidates"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn full_id_remains_compatible_for_project_tools() {
    let runtime = runtime_with_resolver_projects().await;
    let bootstrap = auth_context(None, true);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFile {
                        project: "agent:workstation:other-repo".to_string(),
                        path: "README.md".to_string(),
                        session_id: None,
                        start_line: None,
                        limit: None,
                        with_line_numbers: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_runner_request_for_client(&runtime, "workstation").await;
    runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: "workstation".to_string(),
            runner_instance_id: "inst-workstation".to_string(),
            request_id: req.request_id,
            exit_code: Some(0),
            stdout: Some(canonical_agent_file_read_output("hello\n", 1)),
            stderr: None,
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap();
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
}
