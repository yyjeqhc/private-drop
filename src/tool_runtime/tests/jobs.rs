//! Jobs tests for tool_runtime.

use super::super::helpers::*;
use super::super::kernel::{ToolCallContext, ToolCallRequest, ToolTransport};
use super::super::ToolRuntime;
use super::super::*;
use super::support::*;
use crate::runner_protocol::{
    RunnerCapabilities, RunnerJobUpdateRequest, RunnerRegisterRequest, RunnerResultPayload,
    RunnerResultRequest, ShellCommandExecutionState,
};
use serde_json::json;

fn run_jobs_test_in_large_stack_thread<F, Fut>(test: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    // Some end-to-end Job fixtures retain several auth identities, Job handles,
    // and the generic dispatch future across many awaits. Keep that integration
    // stack local to the fixture instead of raising RUST_MIN_STACK for the suite.
    let result = std::thread::Builder::new()
        .name("runtime-job-test".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build isolated runtime Job test runtime")
                .block_on(test());
        })
        .expect("spawn isolated runtime Job test thread")
        .join();
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[tokio::test]
async fn run_shell_session_events_record_exit_without_stdio_bodies() {
    let runtime = runtime_with_agent_project("telemetry-shell");
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, "telemetry-shell", None, caps).await;
    let project = agent_test_project_id("telemetry-shell");
    let session = runtime.sessions.start_session(Some(project.clone()), None);

    let ok_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::RunShell {
                        project,
                        command: "printf success-output".to_string(),
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
    let req = wait_for_patch_agent_request(&runtime, "telemetry-shell").await;
    complete_patch_agent_request(
        &runtime,
        "telemetry-shell",
        &req.request_id,
        0,
        "shell-secret-out",
        "shell-secret-err",
    )
    .await;
    let ok = ok_task.await.unwrap();
    assert!(ok.success, "{:?}", ok.error);
    assert!(ok.output.get("session_recorded").is_none());
    assert!(ok.output.get("session_event_id").is_none());
    assert!(ok.output.get("session_id").is_none());
    assert_eq!(ok.output["permission"]["required"], true);
    assert_eq!(ok.output["permission"]["policy"], "trusted_agent");
    assert_eq!(ok.output["permission"]["status"], "auto_approved");
    assert_eq!(ok.output["permission"]["risk"], "shell");

    let fail_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::RunShell {
                        project,
                        command: "printf failure-output; exit 7".to_string(),
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
    let req = wait_for_patch_agent_request(&runtime, "telemetry-shell").await;
    complete_patch_agent_request(
        &runtime,
        "telemetry-shell",
        &req.request_id,
        7,
        "fail-secret-out",
        "fail-secret-err",
    )
    .await;
    let fail = fail_task.await.unwrap();
    assert!(!fail.success);
    assert_eq!(fail.output["failure_kind"], "command_exit_nonzero");
    assert!(fail.output.get("session_recorded").is_none());
    assert!(fail.output.get("session_event_id").is_none());
    assert!(fail.output.get("session_id").is_none());
    assert_eq!(fail.output["permission"]["status"], "auto_approved");

    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    assert_eq!(summary.counts.tool_calls, 2);
    assert_eq!(summary.counts.succeeded, 1);
    assert_eq!(summary.counts.failed, 1);
    assert_eq!(summary.counts.shell_like, 2);
    let permission_summary = crate::tool_runtime::permissions::permission_summary_from_events(
        &summary.events,
        crate::tool_runtime::permissions::DEFAULT_PERMISSION_RECENT_LIMIT,
    );
    assert_eq!(permission_summary["required_count"], 2);
    assert_eq!(permission_summary["auto_approved_count"], 2);
    assert_eq!(permission_summary["manual_approved_count"], 0);
    assert!(permission_summary.get("approved_count").is_none());
    assert_eq!(permission_summary["total_approved_count"], 2);
    let failed = summary
        .events
        .iter()
        .rev()
        .find(|event| {
            event.kind == "tool_call_finished"
                && event.tool_name == "run_shell"
                && event.status.as_deref() == Some("failed")
        })
        .unwrap();
    assert_eq!(failed.exit_code, Some(7));
    assert_eq!(failed.failure_kind.as_deref(), Some("command_exit_nonzero"));
    assert_eq!(failed.error_kind.as_deref(), Some("command_exit_nonzero"));
    let permission = failed.permission.as_ref().expect("permission metadata");
    assert_eq!(permission.status, "auto_approved");
    assert_eq!(permission.risk, "shell");
    let serialized = serde_json::to_string(&summary.events).unwrap();
    for leaked in [
        "shell-secret-out",
        "shell-secret-err",
        "fail-secret-out",
        "fail-secret-err",
    ] {
        assert!(
            !serialized.contains(leaked),
            "session event leaked shell output {leaked}: {serialized}"
        );
    }
    assert!(serialized.contains("\"command_present\":true"));
}

#[test]
fn is_safe_job_id_rejects_path_traversal_and_separators() {
    assert!(is_safe_job_id("11111111-2222-3333-4444-555555555555"));
    assert!(is_safe_job_id("job.1_2-3"));
    assert!(!is_safe_job_id("../escape"));
    assert!(!is_safe_job_id("a/b"));
    assert!(!is_safe_job_id("a\\b"));
    assert!(!is_safe_job_id(".."));
    assert!(!is_safe_job_id("a..b/../c"));
    assert!(!is_safe_job_id(""));
    assert!(!is_safe_job_id("a\0b"));
}

/// Write a log file with `lines` lines of `line N` (1-based) and return the
/// tempdir guard plus the file path.
/// Drive `run_shell` end to end against a freshly registered shell-capable
/// agent: spawn the call, wait for the enqueued agent request, and complete it
/// with the given `(exit_code, stdout, stderr)`. Passing `completion: None`
/// leaves the request unanswered (e.g. to exercise the run_shell timeout).
async fn run_shell_via_agent(
    client_id: &str,
    command: &str,
    timeout_secs: Option<u64>,
    completion: Option<(i32, &str, &str)>,
) -> ToolResult {
    let runtime = runtime_with_agent_project(client_id);
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, client_id, None, caps).await;
    let project = agent_test_project_id(client_id);
    let runtime_for_task = runtime.clone();
    let command = command.to_string();
    let task = tokio::spawn(async move {
        runtime_for_task
            .run_shell(project, command, timeout_secs, None)
            .await
    });
    let req = wait_for_patch_agent_request(&runtime, client_id).await;
    if let Some((exit_code, stdout, stderr)) = completion {
        complete_patch_agent_request(
            &runtime,
            client_id,
            &req.request_id,
            exit_code,
            stdout,
            stderr,
        )
        .await;
    }
    task.await.unwrap()
}

async fn run_shell_via_agent_lifecycle_error(
    client_id: &str,
    error: &str,
    execution_state: ShellCommandExecutionState,
) -> ToolResult {
    let runtime = runtime_with_agent_project(client_id);
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, client_id, None, caps).await;
    let project = agent_test_project_id(client_id);
    let runtime_for_task = runtime.clone();
    let task = tokio::spawn(async move {
        runtime_for_task
            .run_shell(project, "printf lifecycle".to_string(), Some(30), None)
            .await
    });
    let request = wait_for_patch_agent_request(&runtime, client_id).await;
    runtime
        .runner_registry
        .complete(RunnerResultPayload {
            result: RunnerResultRequest {
                client_id: client_id.to_string(),
                runner_instance_id: "inst".to_string(),
                request_id: request.request_id,
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(1),
                error: Some(error.to_string()),
            },
            command_execution_state: Some(execution_state),
            mcp_gateway: None,
            plugin_gateway: None,
            coding_agent: None,
        })
        .await
        .unwrap();
    task.await.unwrap()
}

async fn update_agent_shell_job(
    runtime: &ToolRuntime,
    client_id: &str,
    request_id: &str,
    job_id: &str,
    status: &str,
    command_execution_state: Option<ShellCommandExecutionState>,
    exit_code: Option<i32>,
    stdout_chunk: Option<&str>,
    stderr_chunk: Option<&str>,
    error: Option<&str>,
    finished: bool,
) {
    runtime
        .runner_registry
        .update_job(RunnerJobUpdateRequest {
            client_id: client_id.to_string(),
            runner_instance_id: "inst".to_string(),
            job_id: job_id.to_string(),
            request_id: Some(request_id.to_string()),
            update_seq: None,
            status: status.to_string(),
            stdout_chunk: stdout_chunk.map(str::to_string),
            stderr_chunk: stderr_chunk.map(str::to_string),
            stdout_tail: None,
            stderr_tail: None,
            log_snapshot: None,
            exit_code,
            duration_ms: finished.then_some(25),
            error: error.map(str::to_string),
            command_execution_state,
            validation_progress: None,
            activity: None,
            finished,
        })
        .await
        .unwrap();
}

fn assert_run_shell_result_matches_schema(result: &ToolResult) {
    let schema = super::super::registry::output_schema_for_tool("run_shell");
    let instance = serde_json::to_value(result).unwrap();
    super::super::startup_brief::validate_schema_instance_for_test(&instance, &schema)
        .unwrap_or_else(|error| {
            panic!("run_shell result did not match output schema: {error}; {instance}")
        });
}

#[test]
fn project_execution_output_schemas_do_not_advertise_server_local_executor() {
    let observe = super::super::registry::output_schema_for_tool("observe_jobs");
    let observation = &observe["properties"]["output"]["anyOf"][0]["properties"]["items"]["items"]
        ["properties"]["output"]["anyOf"][0];
    assert_eq!(observation["properties"]["executor"]["const"], "agent");

    let list = super::super::registry::output_schema_for_tool("list_jobs");
    assert_eq!(
        list["properties"]["output"]["properties"]["jobs"]["items"]["properties"]["executor"]
            ["const"],
        "agent"
    );

    let persistent = super::super::registry::output_schema_for_tool("open_session_shell");
    assert_eq!(
        persistent["properties"]["output"]["properties"]["executor"]["enum"],
        json!(["agent", "ssh"])
    );

    for name in [
        "run_process",
        "run_script",
        "run_shell",
        "run_job",
        "job_log",
        "cargo_fmt",
        "cargo_check",
        "cargo_test",
        "go_test",
    ] {
        let schema = super::super::registry::output_schema_for_tool(name);
        let executor = &schema["properties"]["output"]["properties"]["executor"];
        assert_eq!(
            executor["const"], "agent",
            "{name} must publish the Runner-only Project execution contract"
        );
    }
}

#[tokio::test]
async fn long_run_shell_hands_off_same_job_once_and_status_log_stop_observe_it() {
    let client_id = "shell-long-handoff";
    let project_id = "proj-long-handoff";
    let runtime =
        test_runtime().with_structured_execution_sync_wait(std::time::Duration::from_millis(20));
    let auth = open_auth_context();
    register_job_agent_for_auth(&runtime, client_id, project_id, &auth).await;
    let project = format!("agent:{client_id}:{project_id}");
    let session = runtime.sessions.start_session(
        Some(project.clone()),
        Some("long run_shell durable handoff".to_string()),
    );
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::RunShell {
                        project,
                        command: "printf durable-shell; sleep 30".to_string(),
                        session_id: Some(session_id),
                        timeout_secs: Some(120),
                        cwd: Some(".".to_string()),
                        purpose: Some(ExecutionPurpose::Diagnostic),
                        shell: Some(ExecutionShell::Bash),
                    },
                    Some(&auth),
                )
                .await
        }
    });

    let start = wait_for_patch_agent_request(&runtime, client_id).await;
    assert_eq!(start.kind, "start_job");
    assert_eq!(start.timeout_secs, 120);
    assert!(start.command.starts_with("exec bash -c "));
    let job_id = start.job_id.clone().expect("durable Job id");
    update_agent_shell_job(
        &runtime,
        client_id,
        &start.request_id,
        &job_id,
        "running",
        None,
        None,
        Some("durable-shell\n"),
        None,
        None,
        false,
    )
    .await;

    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["promoted_to_job"], true);
    assert_eq!(result.output["terminal"], false);
    assert_eq!(result.output["execution_state"], "running");
    assert_eq!(result.output["command_started"], true);
    assert_eq!(result.output["command_completed"], false);
    assert_eq!(result.output["effective_timeout_secs"], 120);
    assert_eq!(result.output["sync_wait_secs"], 10);
    assert_eq!(result.output["job_id"], job_id);
    assert_eq!(result.output["purpose"], "diagnostic");
    assert_eq!(result.output["shell"], "bash");
    assert_eq!(result.output["cwd"], ".");
    assert!(result.output["observation_token"].is_string());
    assert!(result.output.as_object().unwrap().contains_key("activity"));
    assert!(result.output["activity"].is_null());
    assert_run_shell_result_matches_schema(&result);
    assert!(
        probe_patch_agent_request(&runtime, client_id)
            .await
            .is_none(),
        "handoff must not redispatch the shell command"
    );

    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .expect("session summary");
    let finished = summary
        .events
        .iter()
        .find(|event| event.kind == "tool_call_finished" && event.tool_name == "run_shell")
        .expect("run_shell finish event");
    assert_eq!(finished.status.as_deref(), Some("succeeded"));
    assert!(finished.failure_kind.is_none());

    let status = runtime
        .dispatch_with_auth(
            ToolCall::JobStatus {
                job_id: job_id.clone(),
                include_command_preview: false,
            },
            Some(&auth),
        )
        .await;
    assert!(status.success, "{:?}", status.error);
    assert_eq!(status.output["job_id"], job_id);
    assert_eq!(status.output["status"], "running");
    assert!(status.output.as_object().unwrap().contains_key("activity"));
    assert!(status.output["activity"].is_null());

    let log = runtime
        .job_log_for_auth(job_id.clone(), None, Some(40), Some(&auth), None, None)
        .await;
    assert!(log.success, "{:?}", log.error);
    assert_eq!(log.output["job_id"], job_id);
    assert!(log.output.as_object().unwrap().contains_key("activity"));
    assert!(log.output["activity"].is_null());
    assert!(log.output["stdout_tail"]
        .as_str()
        .unwrap_or_default()
        .contains("durable-shell"));

    let listed = runtime
        .list_jobs_for_auth_with_filters(None, None, None, None, Some(&auth))
        .await;
    assert!(listed.success, "{:?}", listed.error);
    let listed_job = listed.output["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["job_id"] == job_id)
        .expect("durable shell Job in list_jobs");
    assert!(listed_job.as_object().unwrap().contains_key("activity"));
    assert!(listed_job["activity"].is_null());

    let observed = runtime
        .observe_jobs_for_auth(
            vec![ObserveJobsItem {
                job_id: job_id.clone(),
                after_observation_token: None,
            }],
            40,
            None,
            Some(&auth),
        )
        .await;
    assert!(observed.success, "{:?}", observed.error);
    let observed_job = &observed.output["items"][0]["output"];
    assert!(observed_job.as_object().unwrap().contains_key("activity"));
    assert!(observed_job["activity"].is_null());

    let stopped = runtime
        .dispatch_with_auth(
            ToolCall::StopJob {
                project: project.clone(),
                job_id: job_id.clone(),
                session_id: Some(session.session_id.clone()),
                confirm: true,
            },
            Some(&auth),
        )
        .await;
    assert!(stopped.success, "{:?}", stopped.error);
    let stop_request = wait_for_patch_agent_request(&runtime, client_id).await;
    assert_eq!(stop_request.kind, "stop_job");
    assert_eq!(stop_request.job_id.as_deref(), Some(job_id.as_str()));
    update_agent_shell_job(
        &runtime,
        client_id,
        &start.request_id,
        &job_id,
        "stopped",
        Some(ShellCommandExecutionState::Completed),
        None,
        None,
        None,
        None,
        true,
    )
    .await;
    let terminal = runtime
        .job_status_for_auth(job_id.clone(), false, Some(&auth))
        .await;
    assert!(terminal.success, "{:?}", terminal.error);
    assert_eq!(terminal.output["status"], "stopped");
    assert!(runtime.runner_registry.remove_job_record(&job_id).await);
}

#[tokio::test]
async fn long_run_shell_fast_terminal_returns_ordinary_result_without_visible_job() {
    let client_id = "shell-long-fast";
    let runtime = runtime_with_agent_project(client_id)
        .with_structured_execution_sync_wait(std::time::Duration::from_millis(200));
    register_agent(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            shell: true,
            async_shell_jobs: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id(client_id);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .run_shell(project, "printf fast".to_string(), Some(120), None)
                .await
        }
    });
    let start = wait_for_patch_agent_request(&runtime, client_id).await;
    assert_eq!(start.kind, "start_job");
    let job_id = start.job_id.clone().unwrap();
    update_agent_shell_job(
        &runtime,
        client_id,
        &start.request_id,
        &job_id,
        "completed",
        Some(ShellCommandExecutionState::Completed),
        Some(0),
        Some("fast\n"),
        None,
        None,
        true,
    )
    .await;
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["execution_state"], "completed");
    assert_eq!(result.output["promoted_to_job"], false);
    assert_eq!(result.output["terminal"], true);
    assert_eq!(result.output["job_id"], serde_json::Value::Null);
    assert_eq!(result.output["effective_timeout_secs"], 120);
    assert_eq!(result.output["sync_wait_secs"], 10);
    assert_run_shell_result_matches_schema(&result);
    assert!(runtime.runner_registry.list_jobs(Some(10)).await.is_empty());
}

#[tokio::test]
async fn run_shell_default_sixty_stays_synchronous_even_with_async_job_capability() {
    let client_id = "shell-default-sync";
    let runtime = runtime_with_agent_project(client_id);
    register_agent(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            shell: true,
            async_shell_jobs: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id(client_id);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .run_shell(project, "printf default".to_string(), None, None)
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, client_id).await;
    assert_eq!(request.kind, "run_shell");
    assert_eq!(request.timeout_secs, 60);
    complete_patch_agent_request(&runtime, client_id, &request.request_id, 0, "default", "").await;
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert!(result.output.get("promoted_to_job").is_none());
    assert!(runtime.runner_registry.list_jobs(Some(10)).await.is_empty());
}

#[tokio::test]
async fn long_run_shell_async_job_capability_does_not_bypass_shell_authority() {
    let client_id = "shell-long-no-shell";
    let project_id = "proj-long-no-shell";
    let runtime = test_runtime();
    let auth = open_auth_context();
    runtime
        .runner_registry
        .register_with_auth(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: Some(4),
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: client_id.to_string(),
                runner_instance_id: "inst".to_string(),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: None,
                owner: None,
                hostname: None,
                host_context: None,
                capabilities: crate::test_support::current_runner_capabilities(
                    RunnerCapabilities {
                        shell: false,
                        async_shell_jobs: true,
                        ..Default::default()
                    },
                ),
                policy: None,
            },
            Some(&crate::test_support::runner_access(&auth)),
        )
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        "inst",
        vec![registered_project(
            project_id,
            &format!("/tmp/{project_id}"),
        )],
    )
    .await;
    let project = format!("agent:{client_id}:{project_id}");
    let result = runtime
        .dispatch_with_auth(
            ToolCall::RunShell {
                project,
                command: "printf denied".to_string(),
                session_id: None,
                timeout_secs: Some(120),
                cwd: None,
                purpose: None,
                shell: None,
            },
            Some(&auth),
        )
        .await;
    assert!(!result.success);
    let error = result.error.as_deref().unwrap_or_default();
    assert!(error.contains("does not support shell"), "{error}");
    assert!(error.contains(client_id), "{error}");
    assert!(probe_patch_agent_request(&runtime, client_id)
        .await
        .is_none());
    assert!(runtime.runner_registry.list_jobs(Some(10)).await.is_empty());
}

#[tokio::test]
async fn long_run_shell_job_timeout_is_terminal_and_never_becomes_fake_outcome_unknown() {
    let client_id = "shell-long-timeout";
    let runtime = runtime_with_agent_project(client_id)
        .with_structured_execution_sync_wait(std::time::Duration::from_millis(20));
    register_agent(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            shell: true,
            async_shell_jobs: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id(client_id);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .run_shell(
                    project,
                    "printf partial; sleep 30".to_string(),
                    Some(120),
                    None,
                )
                .await
        }
    });
    let start = wait_for_patch_agent_request(&runtime, client_id).await;
    let job_id = start.job_id.clone().unwrap();
    update_agent_shell_job(
        &runtime,
        client_id,
        &start.request_id,
        &job_id,
        "running",
        None,
        None,
        Some("partial\n"),
        None,
        None,
        false,
    )
    .await;
    let handoff = task.await.unwrap();
    assert!(handoff.success, "{:?}", handoff.error);
    assert_eq!(handoff.output["job_id"], job_id);
    assert_eq!(handoff.output["promoted_to_job"], true);

    update_agent_shell_job(
        &runtime,
        client_id,
        &start.request_id,
        &job_id,
        "timeout",
        Some(ShellCommandExecutionState::TimedOut),
        Some(-1),
        None,
        Some("runner deadline reached\n"),
        Some("command timed out"),
        true,
    )
    .await;
    let status = runtime.job_status(job_id.clone()).await;
    assert!(status.success, "{:?}", status.error);
    assert_eq!(status.output["status"], "timeout");
    assert_eq!(status.output["command_execution_state"], "timed_out");
    assert_ne!(status.output["command_execution_state"], "outcome_unknown");
    let log = runtime.job_log(job_id.clone(), None, Some(40)).await;
    assert!(log.success, "{:?}", log.error);
    assert_eq!(log.output["command_execution_state"], "timed_out");
    assert!(log.output["stderr_tail"]
        .as_str()
        .unwrap_or_default()
        .contains("runner deadline reached"));
    assert!(probe_patch_agent_request(&runtime, client_id)
        .await
        .is_none());
    assert!(runtime.runner_registry.remove_job_record(&job_id).await);
}

#[tokio::test]
async fn run_shell_failure_reports_command_started_and_output_tail() {
    let result = run_shell_via_agent(
        "shell-failer",
        "printf run-shell-out; printf run-shell-err >&2; exit 7",
        Some(30),
        Some((7, "run-shell-out", "run-shell-err")),
    )
    .await;
    assert!(!result.success);
    let error = result.error.as_deref().unwrap_or("");
    assert!(error.contains("Command exited with status 7"));
    assert!(error.contains("No files were modified by WebCodex itself"));
    assert!(error.contains("stdout_tail"));
    assert!(error.contains("stderr_tail"));
    assert!(error.contains("Retry guidance"));
    assert_eq!(result.output["exit_code"], 7);
    assert_eq!(result.output["stdout_tail"], "run-shell-out");
    assert_eq!(result.output["stderr_tail"], "run-shell-err");
    assert_eq!(result.output["command_started"], true);
    assert_eq!(result.output["command_completed"], true);
    assert_eq!(result.output["command_ok"], false);
    assert_eq!(result.output["execution_state"], "completed");
    assert_eq!(result.output["failure_kind"], "command_exit_nonzero");
    assert_eq!(result.output["tool_failure"], false);
}

#[tokio::test]
async fn raw_shell_tools_reject_authored_command_above_shared_bound_before_project_resolution() {
    let command = "x".repeat(crate::runner_protocol::RAW_SHELL_COMMAND_MAX_BYTES + 1);

    let run_shell = test_runtime()
        .run_shell(
            "agent:missing:missing".to_string(),
            command.clone(),
            Some(30),
            None,
        )
        .await;
    assert!(!run_shell.success);
    assert!(run_shell
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("raw shell command exceeds the 16000-byte UTF-8 limit"));
    assert_eq!(run_shell.output["execution_state"], "not_started");
    assert_eq!(run_shell.output["failure_kind"], "runtime_error");

    let run_job = test_runtime()
        .run_job_for_auth(
            "agent:missing:missing".to_string(),
            command,
            None,
            Some(30),
            None,
            Vec::new(),
            Some(&auth_context(None, true)),
        )
        .await;
    assert!(!run_job.success);
    assert!(run_job
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("raw shell command exceeds the 16000-byte UTF-8 limit"));
}

#[tokio::test]
async fn run_shell_rejection_reports_not_started_and_no_files_modified() {
    let result = test_runtime()
        .run_shell(
            "agent:missing:missing".to_string(),
            "printf should-not-run".to_string(),
            Some(30),
            None,
        )
        .await;
    assert!(!result.success);
    let error = result.error.as_deref().unwrap_or("");
    assert!(error.contains("Rejected before starting command"));
    assert!(error.contains("No command was started"));
    assert!(error.contains("No files were modified"));
    assert!(error.contains("Retry guidance"));
    assert_eq!(result.output["command_started"], false);
    assert_eq!(result.output["command_completed"], false);
    assert_eq!(result.output["command_ok"], false);
    assert_eq!(result.output["execution_state"], "not_started");
    assert_eq!(result.output["failure_kind"], "agent_offline");
    assert_eq!(result.output["tool_failure"], true);
}

#[tokio::test]
async fn run_shell_runner_pre_spawn_error_reports_not_started() {
    let result = run_shell_via_agent_lifecycle_error(
        "shell-pre-spawn-error",
        "failed to spawn command: executable not found",
        ShellCommandExecutionState::NotStarted,
    )
    .await;

    assert!(!result.success);
    let error = result.error.as_deref().unwrap_or_default();
    assert_eq!(result.output["command_started"], false);
    assert_eq!(result.output["command_completed"], false);
    assert_eq!(result.output["execution_state"], "not_started");
    assert!(error.contains("No command was started"));
    assert!(error.contains("No files were modified"));
}

#[tokio::test]
async fn run_shell_runner_post_spawn_output_error_reports_unknown() {
    let result = run_shell_via_agent_lifecycle_error(
        "shell-post-spawn-error",
        "stdout reader did not finish before cleanup deadline",
        ShellCommandExecutionState::OutcomeUnknown,
    )
    .await;

    assert!(!result.success);
    let error = result.error.as_deref().unwrap_or_default();
    assert_eq!(result.output["command_started"], true);
    assert_eq!(result.output["command_completed"], false);
    assert_eq!(result.output["execution_state"], "outcome_unknown");
    assert_eq!(result.output["failure_kind"], "outcome_unknown");
    assert!(error.contains("Do not automatically retry"));
    assert!(!error.contains("No command was started"));
    assert!(!error.contains("No files were modified"));
}

#[tokio::test]
async fn run_shell_exit_codes_report_structured_command_results() {
    struct Case {
        name: &'static str,
        client_id: &'static str,
        command: &'static str,
        exit_code: i32,
        stdout: &'static str,
        stderr: &'static str,
        expect_success: bool,
        expect_command_ok: bool,
        // None => failure_kind must be JSON null.
        expect_failure_kind: Option<&'static str>,
        // Success reports full bodies (stdout/stderr); nonzero exits report
        // bounded tails (stdout_tail/stderr_tail).
        stdout_field: &'static str,
        stderr_field: &'static str,
    }
    let cases = [
        Case {
            name: "exit_zero",
            client_id: "shell-ok",
            command: "printf ok; printf err >&2",
            exit_code: 0,
            stdout: "ok",
            stderr: "err",
            expect_success: true,
            expect_command_ok: true,
            expect_failure_kind: None,
            stdout_field: "stdout_tail",
            stderr_field: "stderr_tail",
        },
        Case {
            name: "exit_seven",
            client_id: "shell-seven",
            command: "printf out; printf err >&2; exit 7",
            exit_code: 7,
            stdout: "out",
            stderr: "err",
            expect_success: false,
            expect_command_ok: false,
            expect_failure_kind: Some("command_exit_nonzero"),
            stdout_field: "stdout_tail",
            stderr_field: "stderr_tail",
        },
    ];
    for case in cases {
        let result = run_shell_via_agent(
            case.client_id,
            case.command,
            Some(30),
            Some((case.exit_code, case.stdout, case.stderr)),
        )
        .await;

        assert_eq!(
            result.success, case.expect_success,
            "[{}] success flag, error: {:?}",
            case.name, result.error
        );
        assert_eq!(
            result.output["exit_code"], case.exit_code,
            "[{}] exit_code",
            case.name
        );
        assert_eq!(
            result.output[case.stdout_field], case.stdout,
            "[{}] {}",
            case.name, case.stdout_field
        );
        assert_eq!(
            result.output[case.stderr_field], case.stderr,
            "[{}] {}",
            case.name, case.stderr_field
        );
        assert_eq!(
            result.output["command_started"], true,
            "[{}] command_started",
            case.name
        );
        assert_eq!(
            result.output["command_completed"], true,
            "[{}] command_completed",
            case.name
        );
        assert_eq!(
            result.output["execution_state"], "completed",
            "[{}] execution_state",
            case.name
        );
        assert_eq!(
            result.output["command_ok"], case.expect_command_ok,
            "[{}] command_ok",
            case.name
        );
        match case.expect_failure_kind {
            Some(kind) => assert_eq!(
                result.output["failure_kind"], kind,
                "[{}] failure_kind",
                case.name
            ),
            None => assert!(
                result.output["failure_kind"].is_null(),
                "[{}] failure_kind should be null, got {:?}",
                case.name,
                result.output["failure_kind"]
            ),
        }
        assert_eq!(
            result.output["tool_failure"], false,
            "[{}] tool_failure",
            case.name
        );
    }
}

#[tokio::test]
async fn run_shell_result_wait_timeout_reports_unknown_outcome() {
    // The enqueued agent request is intentionally never completed, so the
    // result wait expires after dispatch without proving the command's final
    // outcome.
    let result = run_shell_via_agent("shell-timeout", "sleep 2", Some(1), None).await;

    assert!(!result.success);
    let error = result.error.as_deref().unwrap_or_default();
    assert!(error.contains("Command execution outcome is unknown"));
    assert!(error.contains("Do not automatically retry"));
    assert!(error.contains("inspect the actual Job, process, service, or target state"));
    assert!(!error.contains("No command was started"));
    assert!(!error.contains("No files were modified"));
    assert_eq!(result.output["command_started"], true);
    assert_eq!(result.output["command_completed"], false);
    assert_eq!(result.output["command_ok"], false);
    assert!(result.output["exit_code"].is_null());
    assert_eq!(result.output["execution_state"], "outcome_unknown");
    assert_eq!(result.output["failure_kind"], "outcome_unknown");
    assert_eq!(result.output["tool_failure"], true);
}

#[tokio::test]
async fn run_shell_runner_timeout_preserves_known_timeout_state() {
    let client_id = "shell-runner-timeout";
    let runtime = runtime_with_agent_project(client_id);
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, client_id, None, caps).await;
    let project = agent_test_project_id(client_id);
    let runtime_for_task = runtime.clone();
    let task = tokio::spawn(async move {
        runtime_for_task
            .run_shell(project, "sleep 2".to_string(), Some(1), None)
            .await
    });
    let request = wait_for_patch_agent_request(&runtime, client_id).await;
    runtime
        .runner_registry
        .complete(RunnerResultPayload {
            result: RunnerResultRequest {
                client_id: client_id.to_string(),
                runner_instance_id: "inst".to_string(),
                request_id: request.request_id,
                exit_code: Some(-1),
                stdout: Some("partial output".to_string()),
                stderr: Some("runner stopped the process at its deadline".to_string()),
                duration_ms: Some(1_000),
                error: Some("runner timeout".to_string()),
            },
            command_execution_state: Some(ShellCommandExecutionState::TimedOut),
            mcp_gateway: None,
            plugin_gateway: None,
            coding_agent: None,
        })
        .await
        .unwrap();

    let result = task.await.unwrap();
    assert!(!result.success);
    assert_eq!(result.output["command_started"], true);
    assert_eq!(result.output["command_completed"], false);
    assert_eq!(result.output["execution_state"], "timed_out");
    assert_eq!(result.output["failure_kind"], "timeout");
    assert_eq!(result.output["tool_failure"], false);
    let error = result.error.as_deref().unwrap_or_default();
    assert!(error.contains("Command timed out after 1s"));
    assert!(error.contains("do not blindly retry"));
    assert!(error.contains("First inspect the actual process, service, and target state"));
}

#[tokio::test]
async fn run_shell_transport_disconnect_after_dispatch_reports_unknown_outcome() {
    let client_id = "shell-disconnect";
    let runtime = runtime_with_agent_project(client_id);
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, client_id, None, caps).await;
    let project = agent_test_project_id(client_id);
    let runtime_for_task = runtime.clone();
    let task = tokio::spawn(async move {
        runtime_for_task
            .run_shell(project, "printf possibly-ran".to_string(), Some(30), None)
            .await
    });
    wait_for_patch_agent_request(&runtime, client_id).await;

    runtime
        .runner_registry
        .reconcile_disconnect(client_id, "inst")
        .await;

    let result = task.await.unwrap();
    let error = result.error.as_deref().unwrap_or_default();
    assert!(!result.success);
    assert_eq!(result.output["command_started"], true);
    assert_eq!(result.output["command_completed"], false);
    assert_eq!(result.output["execution_state"], "outcome_unknown");
    assert_eq!(result.output["failure_kind"], "outcome_unknown");
    assert!(error.contains("Command execution outcome is unknown"));
    assert!(error.contains("Do not automatically retry"));
    assert!(!error.contains("No command was started"));
    assert!(!error.contains("No files were modified"));
}

#[tokio::test]
async fn run_job_rejects_server_configured_project_without_local_spawn() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let runtime = runtime_with_project(root, "demo");
    let result = runtime
        .run_job_for_auth(
            "demo".to_string(),
            "true".to_string(),
            None,
            Some(10),
            None,
            Vec::new(),
            None,
        )
        .await;
    assert!(!result.success);
    assert!(result.error.unwrap().contains("unknown_project"));
}

#[tokio::test]
async fn stop_job_rejects_unsafe_job_id() {
    let runtime = test_runtime();
    let result = runtime.stop_job("../escape".to_string()).await;
    assert!(!result.success);
    assert!(result.error.unwrap().contains("invalid job id"));
}

#[tokio::test]
async fn stop_job_unknown_job_returns_error() {
    let runtime = test_runtime();
    let result = runtime
        .stop_job("55555555-6666-7777-8888-999999999999".to_string())
        .await;
    assert!(!result.success);
    assert!(result.error.unwrap().contains("unknown job"));
}

#[tokio::test]
async fn model_facing_stop_job_requires_confirm_without_stopping_or_approving() {
    let runtime = test_runtime();
    let auth = open_auth_context();
    register_job_agent_for_auth(&runtime, "client-confirm", "proj-confirm", &auth).await;
    let project = "agent:client-confirm:proj-confirm".to_string();
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("stop confirm".to_string()));
    let job_id = start_agent_runtime_job_in_session(
        &runtime,
        "client-confirm",
        "proj-confirm",
        Some(&session.session_id),
        &auth,
    )
    .await;
    let before_stop_summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    let before_stop_permissions = crate::tool_runtime::permissions::permission_summary_from_events(
        &before_stop_summary.events,
        crate::tool_runtime::permissions::DEFAULT_PERMISSION_RECENT_LIMIT,
    );

    let result = runtime
        .dispatch_with_auth(
            ToolCall::StopJob {
                project,
                job_id: job_id.clone(),
                session_id: Some(session.session_id.clone()),
                confirm: false,
            },
            Some(&auth),
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "confirmation_required");
    assert_eq!(result.output["failure_kind"], "confirmation_required");
    assert_eq!(result.output["stop_effect"], "confirmation_required");
    assert_eq!(result.output["stop_request_accepted"], false);
    assert_eq!(result.output["command_started"], false);
    assert!(result.output.get("permission").is_none());
    let status = runtime
        .job_status_for_auth(job_id, false, Some(&auth))
        .await;
    assert!(status.success, "{:?}", status.error);
    assert!(matches!(
        status.output["status"].as_str(),
        Some("queued" | "agent_queued")
    ));
    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    let permissions = crate::tool_runtime::permissions::permission_summary_from_events(
        &summary.events,
        crate::tool_runtime::permissions::DEFAULT_PERMISSION_RECENT_LIMIT,
    );
    assert_eq!(
        permissions["auto_approved_count"], before_stop_permissions["auto_approved_count"],
        "confirm=false must not record an additional permission approval"
    );
}

#[tokio::test]
async fn model_facing_stop_job_allows_unknown_session_with_project_warning() {
    let runtime = test_runtime();
    let auth = open_auth_context();
    register_job_agent_for_auth(&runtime, "client-warning", "proj-warning", &auth).await;
    let job_id = start_agent_runtime_job(&runtime, "client-warning", "proj-warning", &auth).await;

    let result = runtime
        .dispatch_with_auth(
            ToolCall::StopJob {
                project: "agent:client-warning:proj-warning".to_string(),
                job_id,
                session_id: None,
                confirm: true,
            },
            Some(&auth),
        )
        .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        result.output["ownership_basis"],
        "unknown_session_project_only"
    );
    assert_eq!(result.output["warning_kind"], "job_session_unknown");
    assert_eq!(result.output["warnings"][0]["kind"], "job_session_unknown");
}

#[tokio::test]
async fn model_facing_stop_job_rejects_different_session_before_stop() {
    let runtime = test_runtime();
    let auth = open_auth_context();
    register_job_agent_for_auth(&runtime, "client-session", "proj-session", &auth).await;
    let project = "agent:client-session:proj-session".to_string();
    let owner_session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("owner".to_string()));
    let other_session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("other".to_string()));
    let job_id = start_agent_runtime_job_in_session(
        &runtime,
        "client-session",
        "proj-session",
        Some(&owner_session.session_id),
        &auth,
    )
    .await;

    let result = runtime
        .dispatch_with_auth(
            ToolCall::StopJob {
                project,
                job_id,
                session_id: Some(other_session.session_id.clone()),
                confirm: true,
            },
            Some(&auth),
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "job_stop_forbidden");
    assert_eq!(result.output["failure_kind"], "job_stop_forbidden");
    assert_eq!(result.output["command_started"], false);
    assert!(result.output.get("permission").is_none());
}

#[tokio::test]
async fn model_facing_stop_job_rejects_agent_project_mismatch() {
    let runtime = test_runtime();
    let auth = open_auth_context();
    register_job_agent_for_auth(&runtime, "client-alpha", "proj-alpha", &auth).await;
    register_job_agent_for_auth(&runtime, "client-beta", "proj-beta", &auth).await;
    let job_id = start_agent_runtime_job(&runtime, "client-alpha", "proj-alpha", &auth).await;

    let result = runtime
        .dispatch_with_auth(
            ToolCall::StopJob {
                project: "agent:client-beta:proj-beta".to_string(),
                job_id,
                session_id: None,
                confirm: true,
            },
            Some(&auth),
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "job_project_mismatch");
    assert_eq!(result.output["failure_kind"], "job_project_mismatch");
    assert_eq!(result.output["command_started"], false);
    assert!(result.output.get("permission").is_none());
}

#[tokio::test]
async fn model_facing_stop_job_stops_agent_job_with_same_session() {
    let runtime = test_runtime();
    let auth = open_auth_context();
    register_job_agent_for_auth(&runtime, "client-stop", "proj-stop", &auth).await;
    let project = "agent:client-stop:proj-stop".to_string();
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("agent stop".to_string()));
    let run = runtime
        .dispatch_with_auth(
            ToolCall::RunJob {
                project: project.clone(),
                command: "echo queued".to_string(),
                session_id: Some(session.session_id.clone()),
                timeout_secs: None,
                cwd: None,
                purpose: None,
                shell: None,
            },
            Some(&auth),
        )
        .await;
    assert!(run.success, "{:?}", run.error);
    let job_id = run.output["job_id"].as_str().unwrap().to_string();

    let result = runtime
        .dispatch_with_auth(
            ToolCall::StopJob {
                project,
                job_id: job_id.clone(),
                session_id: Some(session.session_id.clone()),
                confirm: true,
            },
            Some(&auth),
        )
        .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["status_before"], "queued");
    assert_eq!(result.output["status_after"], "stopped");
    assert_eq!(result.output["ownership_basis"], "project_and_session");
    assert_eq!(result.output["permission"]["status"], "auto_approved");
    assert_eq!(result.output["permission"]["risk"], "job");
    let status = runtime
        .dispatch_with_auth(
            ToolCall::JobStatus {
                job_id,
                include_command_preview: false,
            },
            Some(&auth),
        )
        .await;
    assert!(status.success, "{:?}", status.error);
    assert_eq!(status.output["status"], "stopped");
}

#[tokio::test]
async fn model_facing_stop_job_reports_requested_and_already_stop_requested() {
    let runtime = test_runtime();
    let auth = open_auth_context();
    register_job_agent_for_auth(&runtime, "client-stop-pending", "proj-stop-pending", &auth).await;
    let project = "agent:client-stop-pending:proj-stop-pending".to_string();
    let session = runtime.sessions.start_session(
        Some(project.clone()),
        Some("agent stop pending".to_string()),
    );
    let run = runtime
        .dispatch_with_auth(
            ToolCall::RunJob {
                project: project.clone(),
                command: "echo stop-pending".to_string(),
                session_id: Some(session.session_id.clone()),
                timeout_secs: None,
                cwd: None,
                purpose: None,
                shell: None,
            },
            Some(&auth),
        )
        .await;
    assert!(run.success, "{:?}", run.error);
    let job_id = run.output["job_id"].as_str().unwrap().to_string();
    let start_req =
        wait_for_runner_request_for_instance(&runtime, "client-stop-pending", "inst").await;
    assert_eq!(start_req.kind, "start_job");

    let result = runtime
        .dispatch_with_auth(
            ToolCall::StopJob {
                project: project.clone(),
                job_id: job_id.clone(),
                session_id: Some(session.session_id.clone()),
                confirm: true,
            },
            Some(&auth),
        )
        .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["already_finished"], false);
    assert_eq!(result.output["already_stop_requested"], false);
    assert_eq!(result.output["status_before"], "agent_queued");
    assert_eq!(result.output["status_after"], "stop_requested");
    assert_eq!(result.output["stop_request_accepted"], true);
    assert_eq!(result.output["target_was_active_at_request"], true);
    assert_eq!(result.output["terminal"], false);
    assert_eq!(result.output["terminal_pending"], true);
    assert!(result.output["final_status"].is_null());
    assert_eq!(result.output["stop_effect"], "requested");

    let status = runtime
        .dispatch_with_auth(
            ToolCall::JobStatus {
                job_id: job_id.clone(),
                include_command_preview: false,
            },
            Some(&auth),
        )
        .await;
    assert!(status.success, "{:?}", status.error);
    assert_eq!(status.output["status"], "stop_requested");
    assert_eq!(status.output["active"], true);
    assert_eq!(status.output["blocking_active"], false);
    assert_eq!(status.output["terminal"], false);
    assert_eq!(status.output["terminal_pending"], true);
    assert_eq!(status.output["command_preview_included"], false);

    let second = runtime
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
    assert!(second.success, "{:?}", second.error);
    assert_eq!(second.output["already_finished"], false);
    assert_eq!(second.output["already_stop_requested"], true);
    assert_eq!(second.output["status_before"], "stop_requested");
    assert_eq!(second.output["status_after"], "stop_requested");
    assert_eq!(second.output["stop_request_accepted"], false);
    assert_eq!(second.output["target_was_active_at_request"], true);
    assert_eq!(second.output["terminal"], false);
    assert_eq!(second.output["terminal_pending"], true);
    assert!(second.output["final_status"].is_null());
    assert_eq!(second.output["stop_effect"], "already_stop_requested");
}

#[tokio::test]
async fn model_facing_stop_job_session_project_mismatch_beats_auto_approve() {
    let runtime = test_runtime();
    let auth = open_auth_context();
    register_job_agent_for_auth(&runtime, "client-one", "proj-one", &auth).await;
    register_job_agent_for_auth(&runtime, "client-two", "proj-two", &auth).await;
    let session = runtime.sessions.start_session(
        Some("agent:client-one:proj-one".to_string()),
        Some("mismatch".to_string()),
    );

    let outcome = runtime
        .call_tool_with_context(
            ToolCallRequest {
                tool_name: "stop_job".to_string(),
                arguments: json!({
                    "project": "agent:client-two:proj-two",
                    "job_id": "wc_job_not_needed",
                    "session_id": session.session_id,
                    "confirm": true,
                }),
            },
            ToolCallContext {
                transport: ToolTransport::Api,
                session_id: None,
                auth: Some(&auth),
                window: None,
                record_oauth_scope_denials: true,
                host_file_import_trust: crate::tool_runtime::kernel::HostFileImportTrust::Untrusted,
            },
        )
        .await;
    let result = outcome.result.expect("tool result");

    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "session_project_mismatch");
    assert_eq!(result.output["failure_kind"], "session_project_mismatch");
    assert_eq!(result.output["command_started"], false);
    assert!(result.output.get("permission").is_none());
    let summary = runtime
        .sessions
        .summary(result.output["session_id"].as_str().unwrap(), Some(20))
        .unwrap();
    let event = summary
        .events
        .iter()
        .rev()
        .find(|event| event.kind == "tool_call_finished" && event.tool_name == "stop_job")
        .expect("stop_job finished event");
    assert_eq!(
        event.error_kind.as_deref(),
        Some("session_project_mismatch")
    );
    assert!(event.permission.is_none());
}

async fn register_job_agent_for_auth(
    runtime: &ToolRuntime,
    client_id: &str,
    project_id: &str,
    auth: &crate::auth::AuthContext,
) {
    let caps = crate::test_support::current_runner_capabilities(RunnerCapabilities {
        async_shell_jobs: true,
        ..Default::default()
    });
    runtime
        .runner_registry
        .register_with_auth(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: Some(4),
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: client_id.to_string(),
                runner_instance_id: "inst".to_string(),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: None,
                owner: None,
                hostname: None,
                host_context: None,
                capabilities: caps,
                policy: None,
            },
            Some(&crate::test_support::runner_access(auth)),
        )
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        "inst",
        vec![registered_project(
            project_id,
            &format!("/tmp/{project_id}"),
        )],
    )
    .await;
}

fn managed_job_auth(username: &str) -> crate::auth::AuthContext {
    let mut auth = auth_context(Some(username), false);
    auth.scopes = vec![
        crate::auth::SCOPE_RUNTIME_READ.to_string(),
        crate::auth::SCOPE_PROJECT_READ.to_string(),
        crate::auth::SCOPE_JOB_RUN.to_string(),
    ];
    auth
}

async fn register_managed_job_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    project_id: &str,
    owner: &str,
) {
    runtime
        .runner_registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: Some(4),
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: client_id.to_string(),
            runner_instance_id: "inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: Some(owner.to_string()),
            hostname: None,
            host_context: None,
            capabilities: crate::test_support::current_runner_capabilities(RunnerCapabilities {
                async_shell_jobs: true,
                ..Default::default()
            }),
            policy: None,
        })
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        "inst",
        vec![registered_project(
            project_id,
            &format!("/tmp/{owner}/{project_id}"),
        )],
    )
    .await;
}

async fn start_agent_runtime_job(
    runtime: &ToolRuntime,
    client_id: &str,
    project_id: &str,
    auth: &crate::auth::AuthContext,
) -> String {
    start_agent_runtime_job_in_session(runtime, client_id, project_id, None, auth).await
}

async fn start_agent_runtime_job_in_session(
    runtime: &ToolRuntime,
    client_id: &str,
    project_id: &str,
    session_id: Option<&str>,
    auth: &crate::auth::AuthContext,
) -> String {
    let result = runtime
        .dispatch_with_auth(
            ToolCall::RunJob {
                project: format!("agent:{client_id}:{project_id}"),
                command: format!("echo {client_id}"),
                session_id: session_id.map(str::to_string),
                timeout_secs: None,
                cwd: None,
                purpose: None,
                shell: None,
            },
            Some(auth),
        )
        .await;
    assert!(result.success, "{:?}", result.error);
    result.output["job_id"].as_str().unwrap().to_string()
}

async fn mark_next_agent_job_running(runtime: &ToolRuntime, client_id: &str) -> String {
    let request = wait_for_runner_request_for_instance(runtime, client_id, "inst").await;
    let job_id = request.job_id.clone().expect("Job request id");
    runtime
        .runner_registry
        .update_job(RunnerJobUpdateRequest {
            client_id: client_id.to_string(),
            runner_instance_id: "inst".to_string(),
            job_id: job_id.clone(),
            request_id: Some(request.request_id),
            update_seq: None,
            status: "running".to_string(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: None,
            stderr_tail: None,
            log_snapshot: None,
            exit_code: None,
            duration_ms: None,
            error: None,
            command_execution_state: None,
            validation_progress: None,
            activity: None,
            finished: false,
        })
        .await
        .unwrap();
    job_id
}

fn listed_job_ids(result: &ToolResult) -> Vec<String> {
    assert!(result.success, "{:?}", result.error);
    result.output["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|job| job["job_id"].as_str().unwrap().to_string())
        .collect()
}

fn assert_unknown_job(result: ToolResult) {
    assert!(!result.success, "unexpected success: {:?}", result.output);
    assert_eq!(result.output["error_kind"], "unknown_job");
    assert_eq!(result.output["failure_kind"], "job_not_found");
    assert_eq!(result.output["state_changed"], false);
    assert_eq!(result.output["recovery_kind"], "reobserve");
    assert_eq!(result.output["recovery_tool"], "list_jobs");
    assert!(
        result.error.unwrap_or_default().contains("unknown job"),
        "unauthorized job lookup should be hidden as unknown"
    );
}

#[tokio::test]
async fn agent_job_log_invalid_token_is_fix_input_not_unknown_job() {
    let runtime = test_runtime();
    let auth = shared_key_auth_context("hash-token");
    register_job_agent_for_auth(&runtime, "client-token", "proj-token", &auth).await;
    let job_id = start_agent_runtime_job(&runtime, "client-token", "proj-token", &auth).await;

    let result = runtime
        .dispatch_with_auth(
            ToolCall::JobLog {
                job_id,
                offset: None,
                tail_lines: None,
                after_observation_token: Some("bad".to_string()),
                wait_secs: Some(1),
            },
            Some(&auth),
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "invalid_observation_token");
    assert_eq!(result.output["failure_kind"], "invalid_arguments");
    assert_eq!(result.output["state_changed"], false);
    assert_eq!(result.output["recovery_kind"], "fix_input");
    assert!(result.output.get("recovery_tool").is_none());
    assert!(result.error.unwrap_or_default().contains("malformed"));
}

#[tokio::test]
async fn managed_user_job_inventory_and_counts_do_not_cross_owner() {
    let runtime = test_runtime();
    let alice = managed_job_auth("alice");
    let bob = managed_job_auth("bob");
    let bootstrap = bootstrap_auth_context();

    register_managed_job_agent(&runtime, "alice-runner", "alice-project", "alice").await;
    register_managed_job_agent(&runtime, "bob-runner", "bob-project", "bob").await;

    let alice_job =
        start_agent_runtime_job(&runtime, "alice-runner", "alice-project", &alice).await;
    let bob_job = start_agent_runtime_job(&runtime, "bob-runner", "bob-project", &bob).await;

    let alice_list = runtime
        .dispatch_with_auth(
            ToolCall::ListJobs {
                limit: None,
                status: None,
                project: None,
                session_id: None,
            },
            Some(&alice),
        )
        .await;
    assert_eq!(listed_job_ids(&alice_list), vec![alice_job.clone()]);
    assert!(!alice_list.output.to_string().contains(&bob_job));

    let hidden_bob_job = runtime
        .dispatch_with_auth(
            ToolCall::JobStatus {
                job_id: bob_job.clone(),
                include_command_preview: false,
            },
            Some(&alice),
        )
        .await;
    assert_unknown_job(hidden_bob_job);

    let alice_status = runtime
        .dispatch_with_auth(
            ToolCall::RuntimeStatus {
                compact: false,
                summary_only: false,
                client_id: None,
            },
            Some(&alice),
        )
        .await;
    assert!(alice_status.success, "{:?}", alice_status.error);
    assert_eq!(alice_status.output["agents"]["count"], 1);
    assert_eq!(alice_status.output["jobs"]["active_count"], 1);
    assert!(!alice_status.output.to_string().contains("bob-runner"));
    assert!(!alice_status.output.to_string().contains(&bob_job));

    let bootstrap_list = runtime
        .dispatch_with_auth(
            ToolCall::ListJobs {
                limit: None,
                status: None,
                project: None,
                session_id: None,
            },
            Some(&bootstrap),
        )
        .await;
    let mut bootstrap_ids = listed_job_ids(&bootstrap_list);
    bootstrap_ids.sort();
    let mut expected = vec![alice_job, bob_job];
    expected.sort();
    assert_eq!(bootstrap_ids, expected);
}

#[test]
fn shared_key_runtime_job_tools_filter_agent_jobs_by_auth_group() {
    run_jobs_test_in_large_stack_thread(
        shared_key_runtime_job_tools_filter_agent_jobs_by_auth_group_body,
    );
}

async fn shared_key_runtime_job_tools_filter_agent_jobs_by_auth_group_body() {
    let runtime = test_runtime();
    let shared_a = shared_key_auth_context("hash-a");
    let shared_b = shared_key_auth_context("hash-b");
    let bridge_a = oauth_bridge_auth_context("hash-a", &[crate::auth::SCOPE_JOB_RUN]);
    let bridge_b = oauth_bridge_auth_context("hash-b", &[crate::auth::SCOPE_JOB_RUN]);
    let open = open_auth_context();
    let bootstrap = bootstrap_auth_context();

    register_job_agent_for_auth(&runtime, "client-a", "proj-a", &shared_a).await;
    register_job_agent_for_auth(&runtime, "client-b", "proj-b", &shared_b).await;
    register_job_agent_for_auth(&runtime, "client-open", "proj-open", &open).await;

    let job_a = start_agent_runtime_job(&runtime, "client-a", "proj-a", &shared_a).await;
    let job_b = start_agent_runtime_job(&runtime, "client-b", "proj-b", &shared_b).await;
    let job_open = start_agent_runtime_job(&runtime, "client-open", "proj-open", &open).await;

    let req = wait_for_runner_request_for_instance(&runtime, "client-b", "inst").await;
    complete_patch_agent_request(&runtime, "client-b", &req.request_id, 0, "b-out", "b-err").await;

    let list_a = runtime
        .dispatch_with_auth(
            ToolCall::ListJobs {
                limit: None,
                status: None,
                project: None,
                session_id: None,
            },
            Some(&shared_a),
        )
        .await;
    assert_eq!(listed_job_ids(&list_a), vec![job_a.clone()]);

    let list_bridge = runtime
        .dispatch_with_auth(
            ToolCall::ListJobs {
                limit: None,
                status: None,
                project: None,
                session_id: None,
            },
            Some(&bridge_a),
        )
        .await;
    assert_eq!(listed_job_ids(&list_bridge), vec![job_a.clone()]);

    let list_bridge_b = runtime
        .dispatch_with_auth(
            ToolCall::ListJobs {
                limit: None,
                status: None,
                project: None,
                session_id: None,
            },
            Some(&bridge_b),
        )
        .await;
    assert_eq!(listed_job_ids(&list_bridge_b), vec![job_b.clone()]);

    let list_open = runtime
        .dispatch_with_auth(
            ToolCall::ListJobs {
                limit: None,
                status: None,
                project: None,
                session_id: None,
            },
            Some(&open),
        )
        .await;
    assert_eq!(listed_job_ids(&list_open), vec![job_open.clone()]);

    let list_bootstrap = runtime
        .dispatch_with_auth(
            ToolCall::ListJobs {
                limit: None,
                status: None,
                project: None,
                session_id: None,
            },
            Some(&bootstrap),
        )
        .await;
    let mut bootstrap_ids = listed_job_ids(&list_bootstrap);
    bootstrap_ids.sort();
    let mut expected = vec![job_a.clone(), job_b.clone(), job_open.clone()];
    expected.sort();
    assert_eq!(bootstrap_ids, expected);

    assert_unknown_job(
        runtime
            .dispatch_with_auth(
                ToolCall::JobStatus {
                    job_id: job_b.clone(),
                    include_command_preview: false,
                },
                Some(&shared_a),
            )
            .await,
    );
    assert_unknown_job(
        runtime
            .dispatch_with_auth(
                ToolCall::JobStatus {
                    job_id: job_a.clone(),
                    include_command_preview: false,
                },
                Some(&bridge_b),
            )
            .await,
    );
    assert_unknown_job(
        runtime
            .dispatch_with_auth(
                ToolCall::JobStatus {
                    job_id: job_b.clone(),
                    include_command_preview: false,
                },
                Some(&bridge_a),
            )
            .await,
    );
    assert_unknown_job(
        runtime
            .dispatch_with_auth(
                ToolCall::JobLog {
                    job_id: job_b.clone(),
                    offset: None,
                    tail_lines: None,
                    after_observation_token: None,
                    wait_secs: None,
                },
                Some(&shared_a),
            )
            .await,
    );
    assert_unknown_job(
        runtime
            .dispatch_with_auth(
                ToolCall::JobTail {
                    job_id: job_b.clone(),
                    tail_lines: None,
                    after_observation_token: None,
                    wait_secs: None,
                },
                Some(&shared_a),
            )
            .await,
    );

    let status_b = runtime
        .dispatch_with_auth(
            ToolCall::JobStatus {
                job_id: job_b.clone(),
                include_command_preview: false,
            },
            Some(&shared_b),
        )
        .await;
    assert!(status_b.success, "{:?}", status_b.error);
    assert_eq!(status_b.output["job_id"], job_b);
    assert!(status_b.output.get("command_preview").is_none());

    let status_b_debug = runtime
        .dispatch_with_auth(
            ToolCall::JobStatus {
                job_id: job_b.clone(),
                include_command_preview: true,
            },
            Some(&shared_b),
        )
        .await;
    assert!(status_b_debug.success, "{:?}", status_b_debug.error);
    assert!(status_b_debug.output["command_preview"]
        .as_str()
        .unwrap()
        .contains("echo client-b"));

    let log_b = runtime
        .dispatch_with_auth(
            ToolCall::JobLog {
                job_id: job_b.clone(),
                offset: None,
                tail_lines: None,
                after_observation_token: None,
                wait_secs: None,
            },
            Some(&shared_b),
        )
        .await;
    assert!(log_b.success, "{:?}", log_b.error);
    assert_eq!(log_b.output["stdout_tail"], "b-out\n");

    let tail_b = runtime
        .dispatch_with_auth(
            ToolCall::JobTail {
                job_id: job_b,
                tail_lines: Some(10),
                after_observation_token: None,
                wait_secs: None,
            },
            Some(&shared_b),
        )
        .await;
    assert!(tail_b.success, "{:?}", tail_b.error);
    assert_eq!(tail_b.output["stdout_tail"], "b-out\n");
}

#[tokio::test]
async fn list_jobs_filters_visible_jobs_by_project_session_and_status_before_limit() {
    let runtime = test_runtime();
    let auth_a = shared_key_auth_context("targeted-jobs-a");
    let auth_b = shared_key_auth_context("targeted-jobs-b");
    register_job_agent_for_auth(&runtime, "target-a", "proj-a", &auth_a).await;
    register_job_agent_for_auth(&runtime, "target-b", "proj-b", &auth_b).await;
    let project_a = "agent:target-a:proj-a".to_string();
    let project_b = "agent:target-b:proj-b".to_string();
    let session_a1 = runtime
        .sessions
        .start_session(Some(project_a.clone()), Some("A1".to_string()));
    let session_a2 = runtime
        .sessions
        .start_session(Some(project_a.clone()), Some("A2".to_string()));
    let session_b1 = runtime
        .sessions
        .start_session(Some(project_b.clone()), Some("B1".to_string()));

    let job_a1_running = start_agent_runtime_job_in_session(
        &runtime,
        "target-a",
        "proj-a",
        Some(&session_a1.session_id),
        &auth_a,
    )
    .await;
    let job_a1_completed = start_agent_runtime_job_in_session(
        &runtime,
        "target-a",
        "proj-a",
        Some(&session_a1.session_id),
        &auth_a,
    )
    .await;
    let job_a2 = start_agent_runtime_job_in_session(
        &runtime,
        "target-a",
        "proj-a",
        Some(&session_a2.session_id),
        &auth_a,
    )
    .await;
    let _job_b1 = start_agent_runtime_job_in_session(
        &runtime,
        "target-b",
        "proj-b",
        Some(&session_b1.session_id),
        &auth_b,
    )
    .await;

    assert_eq!(
        mark_next_agent_job_running(&runtime, "target-a").await,
        job_a1_running
    );
    let completed_request =
        wait_for_runner_request_for_instance(&runtime, "target-a", "inst").await;
    assert_eq!(
        completed_request.job_id.as_deref(),
        Some(job_a1_completed.as_str())
    );
    complete_patch_agent_request(
        &runtime,
        "target-a",
        &completed_request.request_id,
        0,
        "LIST_JOBS_PRIVATE_STDOUT",
        "LIST_JOBS_PRIVATE_STDERR",
    )
    .await;

    let by_project = runtime
        .dispatch_with_auth(
            ToolCall::ListJobs {
                limit: None,
                status: None,
                project: Some(project_a.clone()),
                session_id: None,
            },
            Some(&auth_a),
        )
        .await;
    assert!(by_project.success, "{:?}", by_project.error);
    assert_eq!(by_project.output["matched_count"], 3);
    assert_eq!(by_project.output["count"], 3);
    let serialized = serde_json::to_string(&by_project.output).unwrap();
    for forbidden in [
        "LIST_JOBS_PRIVATE_STDOUT",
        "LIST_JOBS_PRIVATE_STDERR",
        "stdout",
        "stderr",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "list_jobs leaked log body/key {forbidden}: {serialized}"
        );
    }

    let running = runtime
        .dispatch_with_auth(
            ToolCall::ListJobs {
                limit: None,
                status: Some("running".to_string()),
                project: Some(project_a.clone()),
                session_id: None,
            },
            Some(&auth_a),
        )
        .await;
    assert_eq!(listed_job_ids(&running), vec![job_a1_running.clone()]);

    let a1 = runtime
        .dispatch_with_auth(
            ToolCall::ListJobs {
                limit: None,
                status: None,
                project: Some(project_a.clone()),
                session_id: Some(session_a1.session_id.clone()),
            },
            Some(&auth_a),
        )
        .await;
    let mut a1_ids = listed_job_ids(&a1);
    a1_ids.sort();
    let mut expected_a1 = vec![job_a1_running.clone(), job_a1_completed.clone()];
    expected_a1.sort();
    assert_eq!(a1_ids, expected_a1);

    let a2 = runtime
        .dispatch_with_auth(
            ToolCall::ListJobs {
                limit: None,
                status: None,
                project: None,
                session_id: Some(session_a2.session_id.clone()),
            },
            Some(&auth_a),
        )
        .await;
    assert_eq!(listed_job_ids(&a2), vec![job_a2.clone()]);

    let completed = runtime
        .dispatch_with_auth(
            ToolCall::ListJobs {
                limit: None,
                status: Some("completed".to_string()),
                project: Some(project_a.clone()),
                session_id: Some(session_a1.session_id.clone()),
            },
            Some(&auth_a),
        )
        .await;
    assert_eq!(listed_job_ids(&completed), vec![job_a1_completed.clone()]);

    let limited = runtime
        .dispatch_with_auth(
            ToolCall::ListJobs {
                limit: Some(1),
                status: None,
                project: Some(project_a.clone()),
                session_id: Some(session_a1.session_id.clone()),
            },
            Some(&auth_a),
        )
        .await;
    assert!(limited.success);
    assert_eq!(limited.output["matched_count"], 2);
    assert_eq!(limited.output["count"], 1);
    assert_eq!(limited.output["truncated"], true);

    let mismatched = runtime
        .dispatch_with_auth(
            ToolCall::ListJobs {
                limit: None,
                status: None,
                project: Some(project_a),
                session_id: Some(session_b1.session_id.clone()),
            },
            Some(&auth_a),
        )
        .await;
    assert!(mismatched.success);
    assert_eq!(mismatched.output["count"], 0);

    for foreign_filter in [
        ToolCall::ListJobs {
            limit: None,
            status: None,
            project: Some(project_b),
            session_id: None,
        },
        ToolCall::ListJobs {
            limit: None,
            status: None,
            project: None,
            session_id: Some(session_b1.session_id),
        },
    ] {
        let hidden = runtime
            .dispatch_with_auth(foreign_filter, Some(&auth_a))
            .await;
        assert!(hidden.success, "{:?}", hidden.error);
        assert_eq!(hidden.output["count"], 0);
        assert_eq!(hidden.output["matched_count"], 0);
    }

    let status = runtime
        .dispatch_with_auth(
            ToolCall::JobStatus {
                job_id: job_a1_running,
                include_command_preview: false,
            },
            Some(&auth_a),
        )
        .await;
    assert_eq!(status.output["status"], "running");
}

#[tokio::test]
async fn runtime_status_and_list_runners_filter_concurrency_counts_by_auth_group() {
    let runtime = test_runtime();
    let shared_a = shared_key_auth_context("hash-a");
    let shared_b = shared_key_auth_context("hash-b");
    let open = open_auth_context();
    let bootstrap = bootstrap_auth_context();

    register_job_agent_for_auth(&runtime, "status-a", "proj-a", &shared_a).await;
    register_job_agent_for_auth(&runtime, "status-b", "proj-b", &shared_b).await;
    register_job_agent_for_auth(&runtime, "status-open", "proj-open", &open).await;

    let job_a_running = start_agent_runtime_job(&runtime, "status-a", "proj-a", &shared_a).await;
    assert_eq!(
        mark_next_agent_job_running(&runtime, "status-a").await,
        job_a_running
    );
    let _job_a_queued = start_agent_runtime_job(&runtime, "status-a", "proj-a", &shared_a).await;
    let job_b_running = start_agent_runtime_job(&runtime, "status-b", "proj-b", &shared_b).await;
    assert_eq!(
        mark_next_agent_job_running(&runtime, "status-b").await,
        job_b_running
    );
    let _job_b_queued = start_agent_runtime_job(&runtime, "status-b", "proj-b", &shared_b).await;
    let _job_open = start_agent_runtime_job(&runtime, "status-open", "proj-open", &open).await;

    let status_a = runtime
        .dispatch_with_auth(
            ToolCall::RuntimeStatus {
                compact: false,
                summary_only: false,
                client_id: None,
            },
            Some(&shared_a),
        )
        .await;
    assert!(status_a.success, "{:?}", status_a.error);
    assert_eq!(status_a.output["jobs"]["count"], 2);
    assert!(status_a.output["jobs"].get("agent_known_count").is_none());
    assert_eq!(status_a.output["jobs"]["active_count"], 2);
    assert_eq!(status_a.output["jobs"]["running_count"], 1);
    assert_eq!(status_a.output["jobs"]["queued_count"], 1);
    assert_eq!(
        status_a.output["agents"]["clients"][0]["job_concurrency"],
        json!({"limit": 4, "running": 1, "queued": 1})
    );
    assert!(status_a.output["agents"]["clients"][0]
        .get("available_slots")
        .is_none());
    assert!(status_a.output["agents"]["clients"][0]
        .get("saturated")
        .is_none());

    let agents_a = runtime
        .dispatch_with_auth(
            ToolCall::ListRunners {
                client_id: None,
                client_ids: None,
                include_projects: None,
                summary_only: false,
            },
            Some(&shared_a),
        )
        .await;
    assert!(agents_a.success, "{:?}", agents_a.error);
    assert_eq!(agents_a.output["count"], 1);
    assert_eq!(agents_a.output["agents"][0]["client_id"], "status-a");
    assert_eq!(
        agents_a.output["agents"][0]["job_concurrency"],
        json!({"limit": 4, "running": 1, "queued": 1})
    );
    assert_eq!(
        agents_a.output["clients"][0]["job_concurrency"],
        json!({"limit": 4, "running": 1, "queued": 1})
    );
    let new_observability = agents_a.output["agents"][0]["job_concurrency"]
        .as_object()
        .unwrap();
    assert_eq!(new_observability.len(), 3);
    for forbidden in [
        "stdout",
        "stderr",
        "script",
        "argv",
        "stdin",
        "command",
        "token",
        "credentials",
    ] {
        assert!(new_observability.get(forbidden).is_none(), "{forbidden}");
    }

    let status_open = runtime
        .dispatch_with_auth(
            ToolCall::RuntimeStatus {
                compact: false,
                summary_only: false,
                client_id: None,
            },
            Some(&open),
        )
        .await;
    assert!(status_open.success, "{:?}", status_open.error);
    assert_eq!(status_open.output["jobs"]["count"], 1);
    assert_eq!(status_open.output["jobs"]["active_count"], 1);
    assert_eq!(status_open.output["jobs"]["running_count"], 0);
    assert_eq!(status_open.output["jobs"]["queued_count"], 1);

    let status_bootstrap = runtime
        .dispatch_with_auth(
            ToolCall::RuntimeStatus {
                compact: false,
                summary_only: false,
                client_id: None,
            },
            Some(&bootstrap),
        )
        .await;
    assert!(status_bootstrap.success, "{:?}", status_bootstrap.error);
    assert_eq!(status_bootstrap.output["jobs"]["count"], 5);
    assert_eq!(status_bootstrap.output["jobs"]["active_count"], 5);
    assert_eq!(status_bootstrap.output["jobs"]["running_count"], 2);
    assert_eq!(status_bootstrap.output["jobs"]["queued_count"], 3);
    assert_eq!(status_bootstrap.output["agents"]["count"], 3);

    let compact_a = runtime
        .dispatch_with_auth(
            ToolCall::RuntimeStatus {
                compact: true,
                summary_only: false,
                client_id: None,
            },
            Some(&shared_a),
        )
        .await;
    assert_eq!(
        compact_a.output["jobs"],
        json!({"active_count": 2, "running_count": 1, "queued_count": 1})
    );
}

#[tokio::test]
async fn runtime_concurrency_counts_cover_all_visible_jobs_beyond_list_pagination() {
    let runtime = test_runtime();
    let auth = bootstrap_auth_context();
    register_job_agent_for_auth(&runtime, "count-all", "proj-all", &auth).await;
    for _ in 0..21 {
        start_agent_runtime_job(&runtime, "count-all", "proj-all", &auth).await;
    }

    let status = runtime
        .dispatch_with_auth(
            ToolCall::RuntimeStatus {
                compact: false,
                summary_only: false,
                client_id: None,
            },
            Some(&auth),
        )
        .await;
    assert_eq!(status.output["jobs"]["count"], 21);
    assert_eq!(status.output["jobs"]["active_count"], 21);
    assert_eq!(status.output["jobs"]["running_count"], 0);
    assert_eq!(status.output["jobs"]["queued_count"], 21);
    assert_eq!(
        status.output["agents"]["clients"][0]["job_concurrency"],
        json!({"limit": 4, "running": 0, "queued": 21})
    );
}

#[tokio::test]
async fn list_jobs_invalid_filters_are_fix_input() {
    let runtime = test_runtime();
    for (call, error_kind) in [
        (
            ToolCall::ListJobs {
                limit: None,
                status: None,
                project: Some(" ".to_string()),
                session_id: None,
            },
            "invalid_project_filter",
        ),
        (
            ToolCall::ListJobs {
                limit: None,
                status: None,
                project: None,
                session_id: Some(" ".to_string()),
            },
            "invalid_session_filter",
        ),
    ] {
        let result = runtime.dispatch(call).await;
        assert!(!result.success);
        assert_eq!(result.output["error_kind"], error_kind);
        assert_eq!(result.output["failure_kind"], "invalid_arguments");
        assert_eq!(result.output["state_changed"], false);
        assert_eq!(result.output["recovery_kind"], "fix_input");
        assert!(result.output.get("recovery_tool").is_none());
    }
}

#[tokio::test]
async fn list_jobs_requires_no_runner_capability() {
    // list_jobs has no project and no Runner capability requirement, so it
    // succeeds even with no registered Runner.
    let runtime = test_runtime();
    let result = runtime
        .dispatch(ToolCall::ListJobs {
            limit: None,
            status: None,
            project: None,
            session_id: None,
        })
        .await;
    assert!(result.success);
    assert!(result.output["jobs"].is_array());
}

#[tokio::test]
async fn job_tail_reaches_job_logic_without_agent_auth() {
    // job_tail bypasses agent authorization (no project). An unknown job
    // returns a structured "unknown job" error, proving it reached the job
    // layer rather than an authorization gate.
    let runtime = test_runtime();
    let result = runtime
        .dispatch(ToolCall::JobTail {
            job_id: "no-such-job".to_string(),
            tail_lines: None,
            after_observation_token: None,
            wait_secs: None,
        })
        .await;
    assert_unknown_job(result);
}

// ============================================================================
// Bounded `job_log` input contracts — Runner-owned Jobs
// ============================================================================

#[tokio::test]
async fn job_log_wait_rejects_invalid_wait_secs_before_execution() {
    let runtime = test_runtime();
    for invalid in [0u64, 61u64] {
        let result = runtime
            .dispatch_with_auth(
                ToolCall::JobLog {
                    job_id: "11111111-2222-3333-4444-555555555555".to_string(),
                    offset: None,
                    tail_lines: None,
                    after_observation_token: Some("bad".to_string()),
                    wait_secs: Some(invalid),
                },
                None,
            )
            .await;
        assert!(!result.success);
        assert_eq!(result.output["error_kind"], "invalid_wait_secs");
        assert_eq!(result.output["failure_kind"], "invalid_arguments");
        assert_eq!(result.output["state_changed"], false);
        assert_eq!(result.output["recovery_kind"], "fix_input");
        assert!(result.output.get("recovery_tool").is_none());
        assert!(result.error.as_deref().unwrap_or("").contains("wait_secs"));
    }
}

#[test]
fn job_log_parses_opaque_observation_token_and_rejects_non_string_values() {
    let token =
        crate::job_observation::JobObservationToken::new_legacy("abc", "0123456789abcdef", 7)
            .unwrap()
            .encode();
    let parsed = ToolCall::from_tool_name(
        "job_log",
        json!({"job_id": "abc", "after_observation_token": token, "wait_secs": 5}),
    )
    .unwrap();
    match parsed {
        ToolCall::JobLog {
            after_observation_token,
            wait_secs,
            ..
        } => {
            assert_eq!(after_observation_token.as_deref(), Some(token.as_str()));
            assert_eq!(wait_secs, Some(5));
        }
        other => panic!("expected JobLog, got {other:?}"),
    }

    let result = ToolCall::from_tool_name(
        "job_log",
        json!({"job_id": "abc", "after_observation_token": 1, "wait_secs": 5}),
    );
    assert!(result.is_err());
}
