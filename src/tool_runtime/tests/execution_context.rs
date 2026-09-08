//! Persistent Workflow Session execution-context integration tests.

use super::super::*;
use super::support::*;
use crate::runner_protocol::{
    RunnerCapabilities, RunnerResultPayload, RunnerResultRequest, ShellCommandExecutionState,
};

fn context(cwd: Option<&str>, shell: Option<ExecutionShell>) -> sessions::SessionExecutionContext {
    sessions::SessionExecutionContext {
        default_cwd: cwd.map(str::to_string),
        default_shell: shell,
        resource: None,
    }
}

fn ssh_context(resource: &str, cwd: Option<&str>) -> sessions::SessionExecutionContext {
    sessions::SessionExecutionContext {
        default_cwd: cwd.map(str::to_string),
        default_shell: None,
        resource: Some(resource.to_string()),
    }
}

#[tokio::test]
async fn run_shell_inherits_session_context_and_explicit_arguments_override_it() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let frontend = root.join("frontend");
    let override_dir = root.join("override");
    std::fs::create_dir_all(&frontend).unwrap();
    std::fs::create_dir_all(&override_dir).unwrap();

    let runtime = test_runtime();
    let project = register_runner_project_at_path(&runtime, "context-shell", "demo", &root).await;
    let auth = auth_context(None, true);
    let session = runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(project.clone()),
                Some("context inheritance".to_string()),
                SessionMode::Normal,
                sessions::SessionGuards::default(),
            )
            .with_execution_context(context(Some("frontend"), Some(ExecutionShell::Bash))),
        )
        .unwrap();

    let inherited_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::RunShell {
                        project,
                        command: "pwd".to_string(),
                        session_id: Some(session_id),
                        timeout_secs: Some(30),
                        cwd: None,
                        purpose: None,
                        shell: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let inherited_request = wait_for_patch_agent_request(&runtime, "context-shell").await;
    assert_eq!(
        inherited_request.cwd.as_deref(),
        Some(frontend.to_string_lossy().as_ref())
    );
    assert!(inherited_request.command.starts_with("exec bash -c "));
    complete_patch_agent_request(
        &runtime,
        "context-shell",
        &inherited_request.request_id,
        0,
        "",
        "",
    )
    .await;
    let inherited = inherited_task.await.unwrap();
    assert!(inherited.success, "{:?}", inherited.error);
    assert_eq!(inherited.output["cwd"], "frontend");
    assert_eq!(inherited.output["shell"], "bash");

    let override_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::RunShell {
                        project,
                        command: "pwd".to_string(),
                        session_id: Some(session_id),
                        timeout_secs: Some(30),
                        cwd: Some("override".to_string()),
                        purpose: None,
                        shell: Some(ExecutionShell::Sh),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let override_request = wait_for_patch_agent_request(&runtime, "context-shell").await;
    assert_eq!(
        override_request.cwd.as_deref(),
        Some(override_dir.to_string_lossy().as_ref())
    );
    assert!(override_request.command.starts_with("exec sh -c "));
    complete_patch_agent_request(
        &runtime,
        "context-shell",
        &override_request.request_id,
        0,
        "",
        "",
    )
    .await;
    let overridden = override_task.await.unwrap();
    assert!(overridden.success, "{:?}", overridden.error);
    assert_eq!(overridden.output["cwd"], "override");
    assert_eq!(overridden.output["shell"], "sh");

    let no_session_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::RunShell {
                        project,
                        command: "pwd".to_string(),
                        session_id: None,
                        timeout_secs: Some(30),
                        cwd: None,
                        purpose: None,
                        shell: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let no_session_request = wait_for_patch_agent_request(&runtime, "context-shell").await;
    assert_eq!(
        no_session_request.cwd.as_deref(),
        Some(root.to_string_lossy().as_ref())
    );
    assert_eq!(no_session_request.command, "pwd");
    complete_patch_agent_request(
        &runtime,
        "context-shell",
        &no_session_request.request_id,
        0,
        "",
        "",
    )
    .await;
    let no_session = no_session_task.await.unwrap();
    assert!(no_session.success, "{:?}", no_session.error);
    assert_eq!(no_session.output["cwd"], ".");
    assert_eq!(no_session.output["shell"], "configured");
}

#[tokio::test]
async fn outer_recorder_does_not_override_business_session_execution_context() {
    use crate::tool_runtime::kernel::{
        HostFileImportTrust, ToolCallContext, ToolCallRequest, ToolTransport,
    };

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let recorder_dir = root.join("recorder");
    let business_dir = root.join("business");
    std::fs::create_dir_all(&recorder_dir).unwrap();
    std::fs::create_dir_all(&business_dir).unwrap();

    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "context-recorder", "demo", &root).await;
    let auth = auth_context(None, true);
    let recorder = runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(project.clone()),
                Some("outer recorder".to_string()),
                SessionMode::ReadOnly,
                sessions::SessionGuards::default(),
            )
            .with_execution_context(context(Some("recorder"), Some(ExecutionShell::Sh))),
        )
        .unwrap();
    let business = runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(project.clone()),
                Some("business session".to_string()),
                SessionMode::Normal,
                sessions::SessionGuards::default(),
            )
            .with_execution_context(context(Some("business"), Some(ExecutionShell::Bash))),
        )
        .unwrap();

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
                        tool_name: "run_shell".to_string(),
                        arguments: serde_json::json!({
                            "project": project,
                            "command": "pwd",
                            "session_id": business_id,
                            "timeout_secs": 30
                        }),
                    },
                    ToolCallContext {
                        transport: ToolTransport::Api,
                        session_id: Some(&recorder_id),
                        auth: Some(&auth),
                        window: None,
                        record_oauth_scope_denials: true,
                        host_file_import_trust: HostFileImportTrust::Untrusted,
                    },
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "context-recorder").await;
    assert_eq!(
        request.cwd.as_deref(),
        Some(business_dir.to_string_lossy().as_ref()),
        "business Session cwd must win over recorder context"
    );
    assert!(
        request.command.starts_with("exec bash -c "),
        "business Session shell must win over recorder context: {}",
        request.command
    );
    complete_patch_agent_request(&runtime, "context-recorder", &request.request_id, 0, "", "")
        .await;
    let outcome = task.await.unwrap();
    assert!(outcome.success, "{:?}", outcome.result);
    let result = outcome.result.unwrap();
    assert_eq!(result.output["cwd"], "business");
    assert_eq!(result.output["shell"], "bash");
    assert!(result.output.get("recording_session_project").is_none());
    assert!(result.output.get("recording_session_authorized").is_none());
}

#[tokio::test]
async fn run_job_inherits_session_cwd_and_shell() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let frontend = root.join("frontend");
    std::fs::create_dir_all(&frontend).unwrap();
    let runtime = test_runtime();
    let auth = open_auth_context();
    let capabilities = RunnerCapabilities {
        async_shell_jobs: true,
        ..Default::default()
    };
    register_agent_projects_for_auth(
        &runtime,
        "context-job",
        &auth,
        capabilities,
        vec![registered_project("demo", &root.to_string_lossy())],
    )
    .await;
    let project = "agent:context-job:demo".to_string();
    let session = runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(project.clone()),
                Some("job context".to_string()),
                SessionMode::Normal,
                sessions::SessionGuards::default(),
            )
            .with_execution_context(context(Some("frontend"), Some(ExecutionShell::Bash))),
        )
        .unwrap();

    let result = runtime
        .dispatch_with_auth(
            ToolCall::RunJob {
                project,
                command: "pwd".to_string(),
                session_id: Some(session.session_id),
                timeout_secs: Some(30),
                cwd: None,
                purpose: None,
                shell: None,
            },
            Some(&auth),
        )
        .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["cwd"], "frontend");
    assert_eq!(result.output["shell"], "bash");
    let request = wait_for_runner_request_for_client(&runtime, "context-job").await;
    assert_eq!(request.kind, "start_job");
    assert_eq!(
        request.cwd.as_deref(),
        Some(frontend.to_string_lossy().as_ref())
    );
    assert!(request.command.starts_with("exec bash -c "));
}

#[tokio::test]
async fn session_ssh_resource_uses_remote_cwd_and_safe_agent_context_for_shell_and_jobs() {
    let runtime = test_runtime();
    let auth = open_auth_context();
    let capabilities = RunnerCapabilities {
        ssh_shell: true,
        jobs: true,
        async_jobs: true,
        async_shell_jobs: true,
        ..Default::default()
    };
    register_agent_projects_for_auth(
        &runtime,
        "context-ssh",
        &auth,
        capabilities,
        vec![registered_project("demo", "/runner-local-project")],
    )
    .await;
    let project = "agent:context-ssh:demo".to_string();
    let session = runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(project.clone()),
                Some("SSH context".to_string()),
                SessionMode::Normal,
                sessions::SessionGuards::default(),
            )
            .with_execution_context(ssh_context("tmp", Some("/remote/default"))),
        )
        .unwrap();

    let shell_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::RunShell {
                        project,
                        command: "pwd".to_string(),
                        session_id: Some(session_id),
                        timeout_secs: Some(30),
                        cwd: Some("/remote/override".to_string()),
                        purpose: None,
                        shell: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let shell_request = wait_for_runner_request_for_client(&runtime, "context-ssh").await;
    assert_eq!(shell_request.cwd.as_deref(), Some("/remote/override"));
    let shell_context = shell_request
        .job_context
        .as_ref()
        .expect("SSH shell gets safe execution context");
    assert_eq!(
        shell_context.workflow_session_id.as_deref(),
        Some(session.session_id.as_str())
    );
    assert_eq!(shell_context.ssh_resource.as_deref(), Some("tmp"));
    assert!(shell_context.runtime_project_id.is_none());
    complete_patch_agent_request_for_instance(
        &runtime,
        "context-ssh",
        "inst-context-ssh",
        &shell_request.request_id,
        0,
        "/remote/override\n",
        "",
    )
    .await;
    let shell_result = shell_task.await.unwrap();
    assert!(shell_result.success, "{:?}", shell_result.error);
    assert_eq!(shell_result.output["ssh_resource"], "tmp");
    assert_eq!(shell_result.output["cwd"], "/remote/override");

    let job = runtime
        .dispatch_with_auth(
            ToolCall::RunJob {
                project: project.clone(),
                command: "printf queued".to_string(),
                session_id: Some(session.session_id.clone()),
                timeout_secs: Some(30),
                cwd: None,
                purpose: None,
                shell: None,
            },
            Some(&auth),
        )
        .await;
    assert!(job.success, "{:?}", job.error);
    assert_eq!(job.output["ssh_resource"], "tmp");
    assert_eq!(job.output["cwd"], "/remote/default");
    let job_id = job.output["job_id"].as_str().unwrap().to_string();
    let job_request = wait_for_runner_request_for_client(&runtime, "context-ssh").await;
    assert_eq!(job_request.kind, "start_job");
    assert_eq!(job_request.cwd.as_deref(), Some("/remote/default"));
    assert_eq!(
        job_request
            .job_context
            .as_ref()
            .and_then(|context| context.ssh_resource.as_deref()),
        Some("tmp")
    );
    let status = runtime
        .job_status_for_auth(job_id.clone(), false, Some(&auth))
        .await;
    assert!(status.success, "{:?}", status.error);
    assert_eq!(status.output["ssh_resource"], "tmp");
    let log = runtime
        .job_log_for_auth(job_id.clone(), None, Some(20), Some(&auth), None, None)
        .await;
    assert!(log.success, "{:?}", log.error);
    assert_eq!(log.output["ssh_resource"], "tmp");
    let stopped = runtime
        .dispatch_with_auth(
            ToolCall::StopJob {
                project,
                job_id,
                session_id: Some(session.session_id),
                confirm: true,
            },
            Some(&auth),
        )
        .await;
    assert!(stopped.success, "{:?}", stopped.error);
}

#[tokio::test]
async fn session_ssh_resource_rejects_structured_cargo_before_direct_sync_start() {
    let runtime = test_runtime();
    let auth = open_auth_context();
    let capabilities = RunnerCapabilities {
        ssh_shell: true,
        ..Default::default()
    };
    register_agent_projects_for_auth(
        &runtime,
        "context-ssh-cargo",
        &auth,
        capabilities,
        vec![registered_project("demo", "/runner-local-project")],
    )
    .await;
    let project = "agent:context-ssh-cargo:demo".to_string();
    let session = runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(project.clone()),
                Some("SSH cargo rejection".to_string()),
                SessionMode::Normal,
                sessions::SessionGuards::default(),
            )
            .with_execution_context(ssh_context("tmp", None)),
        )
        .unwrap();

    let result = runtime
        .dispatch_with_auth(
            ToolCall::CargoCheck {
                project,
                session_id: Some(session.session_id),
                cwd: None,
                all_targets: None,
                all_features: None,
                no_default_features: None,
                features: None,
                package: None,
                timeout_secs: Some(30),
                sync_wait_secs: None,
            },
            Some(&auth),
        )
        .await;

    assert!(!result.success);
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("ssh_resource_unsupported_for_request")));
    assert!(
        probe_agent_request_for_client(&runtime, "context-ssh-cargo")
            .await
            .is_none(),
        "structured Cargo rejection must happen before the direct sync command starts"
    );
}

#[tokio::test]
async fn session_ssh_resource_rejects_mutating_cargo_fmt_before_start() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let source = root.join("src/lib.rs");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    let original = "pub fn value()->i32{1}\n";
    std::fs::write(&source, original).unwrap();

    let runtime = test_runtime();
    let auth = open_auth_context();
    let capabilities = RunnerCapabilities {
        ssh_shell: true,
        ..Default::default()
    };
    register_agent_projects_for_auth(
        &runtime,
        "context-ssh-cargo-fmt",
        &auth,
        capabilities,
        vec![registered_project("demo", root.to_string_lossy().as_ref())],
    )
    .await;
    let project = "agent:context-ssh-cargo-fmt:demo".to_string();
    let session = runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(project.clone()),
                Some("SSH mutating cargo fmt rejection".to_string()),
                SessionMode::Normal,
                sessions::SessionGuards::default(),
            )
            .with_execution_context(ssh_context("tmp", None)),
        )
        .unwrap();

    let result = runtime
        .dispatch_with_auth(
            ToolCall::CargoFmt {
                project,
                session_id: Some(session.session_id),
                cwd: None,
                check: None,
                timeout_secs: Some(30),
                sync_wait_secs: None,
            },
            Some(&auth),
        )
        .await;

    assert!(!result.success);
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("ssh_resource_unsupported_for_request")));
    assert!(
        probe_agent_request_for_client(&runtime, "context-ssh-cargo-fmt")
            .await
            .is_none(),
        "mutating cargo fmt rejection must happen before a Runner shell request starts"
    );
    assert_eq!(std::fs::read_to_string(source).unwrap(), original);
}

#[tokio::test]
async fn session_ssh_resource_requires_runner_ssh_shell_capability() {
    let runtime = test_runtime();
    let auth = open_auth_context();
    register_agent_projects_for_auth(
        &runtime,
        "context-ssh-no-capability",
        &auth,
        RunnerCapabilities::default(),
        vec![registered_project("demo", "/runner-local-project")],
    )
    .await;
    let project = "agent:context-ssh-no-capability:demo".to_string();
    let session = runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(project.clone()),
                Some("SSH context without runner capability".to_string()),
                SessionMode::Normal,
                sessions::SessionGuards::default(),
            )
            .with_execution_context(ssh_context("tmp", None)),
        )
        .unwrap();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::RunShell {
                project,
                command: "pwd".to_string(),
                session_id: Some(session.session_id),
                timeout_secs: Some(30),
                cwd: None,
                purpose: None,
                shell: None,
            },
            Some(&auth),
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "agent_capability_unavailable");
    assert_eq!(result.output["recovery_kind"], "none");
    assert!(result.output.get("recovery_tool").is_none());
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("agent_capability_unavailable")));
    assert!(
        probe_agent_request_for_client(&runtime, "context-ssh-no-capability")
            .await
            .is_none(),
        "a Runner without ssh_shell must not receive an SSH resource request"
    );
}

#[tokio::test]
async fn session_ssh_transport_failure_marks_remote_delivery_uncertain() {
    let runtime = test_runtime();
    let auth = open_auth_context();
    let capabilities = RunnerCapabilities {
        ssh_shell: true,
        ..Default::default()
    };
    register_agent_projects_for_auth(
        &runtime,
        "context-ssh-transport",
        &auth,
        capabilities,
        vec![registered_project("demo", "/runner-local-project")],
    )
    .await;
    let project = "agent:context-ssh-transport:demo".to_string();
    let session = runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(project.clone()),
                Some("transport outcome".to_string()),
                SessionMode::Normal,
                sessions::SessionGuards::default(),
            )
            .with_execution_context(ssh_context("tmp", None)),
        )
        .unwrap();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let session_id = session.session_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::RunShell {
                        project,
                        command: "printf uncertain".to_string(),
                        session_id: Some(session_id),
                        timeout_secs: Some(30),
                        cwd: None,
                        purpose: None,
                        shell: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request = wait_for_runner_request_for_client(&runtime, "context-ssh-transport").await;
    runtime
        .runner_registry
        .complete(RunnerResultPayload {
            result: RunnerResultRequest {
                client_id: "context-ssh-transport".to_string(),
                runner_instance_id: "inst-context-ssh-transport".to_string(),
                request_id: request.request_id,
                exit_code: Some(255),
                stdout: Some(String::new()),
                stderr: Some("connection reset".to_string()),
                duration_ms: Some(1),
                error: Some("ssh transport failed after dispatch".to_string()),
            },
            command_execution_state: Some(ShellCommandExecutionState::OutcomeUnknown),
            mcp_gateway: None,
            plugin_gateway: None,
            coding_agent: None,
        })
        .await
        .unwrap();
    let result = task.await.unwrap();
    assert!(!result.success);
    let error = result.error.as_deref().unwrap_or_default();
    assert_eq!(result.output["command_started"], true);
    assert_eq!(result.output["command_completed"], false);
    assert_eq!(result.output["execution_state"], "outcome_unknown");
    assert_eq!(result.output["failure_kind"], "outcome_unknown");
    assert!(error.contains("Command execution outcome is unknown"));
    assert!(error.contains("Do not automatically retry"));
    assert!(error.contains("inspect the actual Job, process, service, or target state"));
    assert!(!error.contains("No command was started"));
    assert!(!error.contains("No files were modified"));
    assert_eq!(result.output["ssh_resource"], "tmp");
}

#[tokio::test]
async fn mismatch_and_invalid_context_fail_closed_without_root_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let first_root = temp.path().join("first");
    let second_root = temp.path().join("second");
    std::fs::create_dir_all(first_root.join("frontend")).unwrap();
    std::fs::create_dir_all(&second_root).unwrap();
    let runtime = test_runtime();
    let first_project =
        register_runner_project_at_path(&runtime, "context-first", "demo", &first_root).await;
    let second_project =
        register_runner_project_at_path(&runtime, "context-second", "demo", &second_root).await;
    let auth = auth_context(None, true);
    let session = runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(first_project.clone()),
                Some("fail closed".to_string()),
                SessionMode::Normal,
                sessions::SessionGuards::default(),
            )
            .with_execution_context(context(Some("frontend"), Some(ExecutionShell::Bash))),
        )
        .unwrap();

    let mismatch = runtime
        .dispatch_with_auth(
            ToolCall::RunShell {
                project: second_project,
                command: "pwd".to_string(),
                session_id: Some(session.session_id.clone()),
                timeout_secs: Some(30),
                cwd: None,
                purpose: None,
                shell: None,
            },
            Some(&auth),
        )
        .await;
    assert!(!mismatch.success);
    assert_eq!(mismatch.output["failure_kind"], "session_project_mismatch");
    assert!(
        probe_patch_agent_request(&runtime, "context-second")
            .await
            .is_none(),
        "mismatched Session must not enqueue with inherited context"
    );

    let before = runtime.sessions.summary(&session.session_id, None).unwrap();
    let invalid = runtime
        .dispatch_with_auth(
            ToolCall::UpdateSessionContext {
                project: first_project,
                session_id: session.session_id.clone(),
                execution_context: context(Some("../outside"), None),
            },
            Some(&auth),
        )
        .await;
    assert!(!invalid.success);
    assert_eq!(invalid.output["error_kind"], "invalid_execution_context");
    let after = runtime.sessions.summary(&session.session_id, None).unwrap();
    assert_eq!(after.execution_context, before.execution_context);
    assert_eq!(after.events_total, before.events_total);
}

#[tokio::test]
async fn nonexistent_inherited_cwd_is_not_retried_at_project_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let runtime = test_runtime();
    let project = register_runner_project_at_path(&runtime, "context-missing", "demo", &root).await;
    let auth = auth_context(None, true);
    let session = runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(project.clone()),
                Some("missing cwd".to_string()),
                SessionMode::Normal,
                sessions::SessionGuards::default(),
            )
            .with_execution_context(context(Some("missing"), None)),
        )
        .unwrap();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::RunShell {
                        project,
                        command: "pwd".to_string(),
                        session_id: Some(session.session_id),
                        timeout_secs: Some(30),
                        cwd: None,
                        purpose: None,
                        shell: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "context-missing").await;
    assert_eq!(
        request.cwd.as_deref(),
        Some(root.join("missing").to_string_lossy().as_ref())
    );
    runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: "context-missing".to_string(),
            runner_instance_id: "inst".to_string(),
            request_id: request.request_id,
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(1),
            error: Some("cwd does not exist".to_string()),
        })
        .await
        .unwrap();
    let result = task.await.unwrap();
    assert!(!result.success);
    assert!(result
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("cwd does not exist"));
    assert!(
        probe_patch_agent_request(&runtime, "context-missing")
            .await
            .is_none(),
        "invalid inherited cwd must not fall back to the project root"
    );
}

#[tokio::test]
async fn update_session_context_requires_authorized_exact_project_and_preserves_state_on_failure() {
    let temp = tempfile::tempdir().unwrap();
    let first_root = temp.path().join("first");
    let second_root = temp.path().join("second");
    std::fs::create_dir_all(&first_root).unwrap();
    std::fs::create_dir_all(&second_root).unwrap();
    let runtime = test_runtime();
    let owner = shared_key_auth_context("context-owner-group");
    let intruder = shared_key_auth_context("context-intruder-group");
    register_agent_projects_for_auth(
        &runtime,
        "context-owner",
        &owner,
        RunnerCapabilities::default(),
        vec![
            registered_project("first", &first_root.to_string_lossy()),
            registered_project("second", &second_root.to_string_lossy()),
        ],
    )
    .await;
    let first_project = "agent:context-owner:first".to_string();
    let second_project = "agent:context-owner:second".to_string();
    let session = runtime
        .sessions
        .start_session(Some(first_project.clone()), Some("update tool".to_string()));

    let set = runtime
        .dispatch_with_auth(
            ToolCall::UpdateSessionContext {
                project: first_project.clone(),
                session_id: session.session_id.clone(),
                execution_context: context(Some("frontend"), Some(ExecutionShell::Bash)),
            },
            Some(&owner),
        )
        .await;
    assert!(set.success, "{:?}", set.error);
    assert_eq!(set.output["project"], first_project);
    assert_eq!(set.output["execution_context"]["default_cwd"], "frontend");
    assert_eq!(set.output["execution_context"]["default_shell"], "bash");
    assert_eq!(
        set.output["previous_execution_context"],
        serde_json::json!({})
    );
    assert_eq!(set.output["changed"], true);

    let before_denied = runtime.sessions.summary(&session.session_id, None).unwrap();
    let denied = runtime
        .dispatch_with_auth(
            ToolCall::UpdateSessionContext {
                project: first_project.clone(),
                session_id: session.session_id.clone(),
                execution_context: sessions::SessionExecutionContext::default(),
            },
            Some(&intruder),
        )
        .await;
    assert!(!denied.success);
    let after_denied = runtime.sessions.summary(&session.session_id, None).unwrap();
    assert_eq!(
        after_denied.execution_context,
        before_denied.execution_context
    );
    assert_eq!(after_denied.events_total, before_denied.events_total);

    let before_mismatch = after_denied;
    let mismatch = runtime
        .dispatch_with_auth(
            ToolCall::UpdateSessionContext {
                project: second_project,
                session_id: session.session_id.clone(),
                execution_context: sessions::SessionExecutionContext::default(),
            },
            Some(&owner),
        )
        .await;
    assert!(!mismatch.success);
    assert_eq!(mismatch.output["error_kind"], "session_project_mismatch");
    assert_eq!(mismatch.output["failure_kind"], "session_project_mismatch");
    assert_eq!(mismatch.output["command_started"], false);
    assert_eq!(mismatch.output["state_changed"], false);
    assert!(mismatch
        .output
        .get("cross_project_escape_supported")
        .is_none());
    assert!(mismatch
        .output
        .get("allow_cross_project_session_required")
        .is_none());
    assert!(mismatch.output.get("allow_cross_project_session").is_none());
    assert!(!mismatch
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("allow_cross_project_session"));
    let after_mismatch = runtime.sessions.summary(&session.session_id, None).unwrap();
    assert_eq!(
        after_mismatch.execution_context,
        before_mismatch.execution_context
    );
    assert_eq!(after_mismatch.events_total, before_mismatch.events_total);

    let clear = runtime
        .dispatch_with_auth(
            ToolCall::UpdateSessionContext {
                project: first_project.clone(),
                session_id: session.session_id.clone(),
                execution_context: sessions::SessionExecutionContext::default(),
            },
            Some(&owner),
        )
        .await;
    assert!(clear.success, "{:?}", clear.error);
    assert_eq!(clear.output["execution_context"], serde_json::json!({}));

    runtime.sessions.close_session(&session.session_id).unwrap();
    let before_closed = runtime.sessions.summary(&session.session_id, None).unwrap();
    let closed = runtime
        .dispatch_with_auth(
            ToolCall::UpdateSessionContext {
                project: first_project.clone(),
                session_id: session.session_id.clone(),
                execution_context: context(Some("frontend"), None),
            },
            Some(&owner),
        )
        .await;
    assert!(!closed.success);
    assert_eq!(closed.output["error_kind"], "session_closed");
    let after_closed = runtime.sessions.summary(&session.session_id, None).unwrap();
    assert_eq!(
        after_closed.execution_context,
        before_closed.execution_context
    );
    assert_eq!(after_closed.events_total, before_closed.events_total);

    let unknown = runtime
        .dispatch_with_auth(
            ToolCall::UpdateSessionContext {
                project: first_project,
                session_id: "wc_sess_unknowncontext01".to_string(),
                execution_context: sessions::SessionExecutionContext::default(),
            },
            Some(&owner),
        )
        .await;
    assert!(!unknown.success);
    assert_eq!(unknown.output["error_kind"], "unknown_session_id");
}
