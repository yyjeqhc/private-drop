//! Model-facing bounded typed script execution contract.

use super::super::*;
use super::support::*;
use crate::runner_protocol::{
    RunnerCapabilities, RunnerJobUpdateRequest, RunnerResultPayload, RunnerResultRequest,
    ShellCommandExecutionState, ShellScriptLanguage, SCRIPT_MAX_BYTES,
};
use crate::tool_runtime::activity::{ActivityRecord, ActivityRecorder};
use crate::tool_runtime::kernel::{ToolCallContext, ToolCallRequest, ToolTransport};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn script_call(
    project: String,
    session_id: Option<String>,
    language: ShellScriptLanguage,
    script: impl Into<String>,
) -> ToolCall {
    ToolCall::RunScript {
        project,
        language,
        script: script.into(),
        args: vec![
            String::new(),
            "two words".to_string(),
            "$(literal)".to_string(),
            "雪".to_string(),
        ],
        stdin: Some("independent stdin\n".to_string()),
        session_id,
        timeout_secs: Some(30),
        sync_wait_secs: None,
        cwd: None,
        purpose: Some(ExecutionPurpose::Operation),
    }
}

fn script_sync_call(
    project: String,
    session_id: Option<String>,
    language: ShellScriptLanguage,
    script: impl Into<String>,
) -> ToolCall {
    let mut call = script_call(project, session_id, language, script);
    let ToolCall::RunScript { sync_wait_secs, .. } = &mut call else {
        unreachable!("script_call must return RunScript");
    };
    *sync_wait_secs = Some(30);
    call
}

async fn register_script_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    root: &std::path::Path,
    structured_script_payload: bool,
) -> String {
    let capabilities = RunnerCapabilities {
        shell: true,
        structured_validation_argv: true,
        structured_process_argv: true,
        structured_script_payload,
        structured_script_javascript: structured_script_payload,
        structured_script_typescript: structured_script_payload,
        ..Default::default()
    };
    register_agent_with_projects(
        runtime,
        client_id,
        None,
        capabilities,
        vec![registered_project("demo", &root.to_string_lossy())],
    )
    .await;
    crate::tool_runtime::runner_project_runtime_id(client_id, "demo")
}

async fn register_script_job_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    root: &std::path::Path,
) -> String {
    let capabilities = RunnerCapabilities {
        shell: true,
        async_jobs: true,
        async_shell_jobs: true,
        structured_validation_argv: true,
        structured_process_argv: true,
        structured_script_payload: true,
        structured_script_javascript: true,
        structured_script_typescript: true,
        structured_execution_jobs: true,
        ..Default::default()
    };
    register_agent_with_projects(
        runtime,
        client_id,
        None,
        capabilities,
        vec![registered_project("demo", &root.to_string_lossy())],
    )
    .await;
    crate::tool_runtime::runner_project_runtime_id(client_id, "demo")
}

async fn update_script_job(
    runtime: &ToolRuntime,
    client_id: &str,
    request: &crate::runner_protocol::RunnerRequest,
    status: &str,
    state: Option<ShellCommandExecutionState>,
    exit_code: Option<i32>,
    stdout: Option<&str>,
    stderr: Option<&str>,
    error: Option<&str>,
) {
    runtime
        .runner_registry
        .update_job(RunnerJobUpdateRequest {
            client_id: client_id.to_string(),
            runner_instance_id: "inst".to_string(),
            job_id: request.job_id.clone().expect("structured Job id"),
            request_id: Some(request.request_id.clone()),
            update_seq: None,
            status: status.to_string(),
            stdout_chunk: stdout.map(str::to_string),
            stderr_chunk: stderr.map(str::to_string),
            stdout_tail: None,
            stderr_tail: None,
            log_snapshot: None,
            exit_code,
            duration_ms: state.map(|_| 25),
            error: error.map(str::to_string),
            command_execution_state: state,
            validation_progress: None,
            activity: None,
            finished: state.is_some(),
        })
        .await
        .unwrap();
}

async fn complete_script_lifecycle(
    runtime: &ToolRuntime,
    client_id: &str,
    request_id: String,
    state: ShellCommandExecutionState,
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    error: Option<&str>,
) {
    runtime
        .runner_registry
        .complete(RunnerResultPayload {
            result: RunnerResultRequest {
                client_id: client_id.to_string(),
                runner_instance_id: "inst".to_string(),
                request_id,
                exit_code,
                stdout: Some(stdout.to_string()),
                stderr: Some(stderr.to_string()),
                duration_ms: Some(9),
                error: error.map(str::to_string),
            },
            command_execution_state: Some(state),
            mcp_gateway: None,
            plugin_gateway: None,
            coding_agent: None,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn run_script_wire_is_typed_body_free_command_and_supports_more_than_32_kib() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project = register_script_agent(&runtime, "script-wire", temp.path(), true).await;
    let mut large_script = "// typed script payload\n".repeat(1_800);
    large_script.push_str("console.log('done');\n");
    assert!(large_script.len() > 32 * 1024);

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let large_script = large_script.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    script_sync_call(project, None, ShellScriptLanguage::Javascript, large_script),
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "script-wire").await;
    assert_eq!(request.kind, "run_script");
    assert_eq!(request.command, "");
    assert!(request.process.is_none());
    let payload = request.script.as_ref().expect("typed script payload");
    assert_eq!(payload.language, ShellScriptLanguage::Javascript);
    assert_eq!(payload.script, large_script);
    assert_eq!(
        payload.args,
        ["", "two words", "$(literal)", "雪"].map(str::to_string)
    );
    assert_eq!(request.stdin.as_deref(), Some("independent stdin\n"));
    assert!(!request.command.contains(&payload.script));

    complete_script_lifecycle(
        &runtime,
        "script-wire",
        request.request_id,
        ShellCommandExecutionState::Completed,
        Some(0),
        "done\n",
        "",
        None,
    )
    .await;
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["language"], "javascript");
    assert_eq!(result.output["stdout_tail"], "done\n");
    for omitted in [
        "execution_source",
        "script_summary",
        "execution_state",
        "command_started",
        "command_completed",
        "command_ok",
        "exit_code",
        "duration_ms",
        "purpose",
        "cwd",
        "executor",
    ] {
        assert!(
            result.output.get(omitted).is_none(),
            "{omitted}: {}",
            result.output
        );
    }
}

#[tokio::test]
async fn run_script_fast_success_projects_back_and_removes_the_hidden_job() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime().with_structured_execution_sync_wait(Duration::from_millis(250));
    let project = register_script_job_agent(&runtime, "script-fast-job", temp.path()).await;
    let auth = auth_context(None, true);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    script_call(
                        project,
                        None,
                        ShellScriptLanguage::Javascript,
                        "console.log('fast');\n",
                    ),
                    Some(&auth),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "script-fast-job").await;
    assert_eq!(request.kind, "start_script_job");
    assert_eq!(request.command, "");
    assert!(request.process.is_none());
    assert!(request.script.is_some());
    update_script_job(
        &runtime,
        "script-fast-job",
        &request,
        "running",
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    update_script_job(
        &runtime,
        "script-fast-job",
        &request,
        "completed",
        Some(ShellCommandExecutionState::Completed),
        Some(0),
        Some("fast\n"),
        None,
        None,
    )
    .await;
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["language"], "javascript");
    for omitted in [
        "execution_state",
        "command_started",
        "command_completed",
        "command_ok",
        "exit_code",
        "duration_ms",
        "purpose",
        "cwd",
        "executor",
        "promoted_to_job",
        "terminal",
        "job_id",
        "job_status",
        "observation_token",
        "effective_timeout_secs",
        "sync_wait_secs",
        "async_handoff_available",
        "failure_kind",
        "tool_failure",
        "stderr_tail",
        "stderr_lines",
        "stdout_truncated",
        "stderr_truncated",
        "script_summary",
        "execution_source",
    ] {
        assert!(
            result.output.get(omitted).is_none(),
            "boring terminal success field {omitted} should be omitted: {}",
            result.output
        );
    }
    let sparse_bytes = serde_json::to_vec(&result.output).unwrap().len();
    assert!(
        sparse_bytes <= 460,
        "boring run_script success regressed above the model-facing context budget: {sparse_bytes} bytes"
    );
    eprintln!("run_script_sparse_terminal_success_bytes={sparse_bytes}");
    let schema = crate::tool_runtime::registry::output_schema_for_tool("run_script");
    let instance = json!({
        "success": true,
        "output": result.output.clone(),
        "error": null,
    });
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&instance, &schema)
        .unwrap_or_else(|error| {
            panic!("sparse terminal success must match run_script schema: {error}")
        });

    assert!(runtime
        .runner_registry
        .hidden_job_ids_for_test()
        .await
        .is_empty());
    assert!(runtime.runner_registry.list_jobs(Some(10)).await.is_empty());
}

#[tokio::test]
async fn run_script_explicit_sync_wait_captures_terminal_after_old_ten_second_threshold() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project =
        register_script_job_agent(&runtime, "script-extended-sync-wait", temp.path()).await;
    let auth = auth_context(None, true);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::RunScript {
                        project,
                        language: ShellScriptLanguage::Sh,
                        script: "printf 'done\\n'\n".to_string(),
                        args: Vec::new(),
                        stdin: None,
                        session_id: None,
                        timeout_secs: Some(60),
                        sync_wait_secs: Some(45),
                        cwd: None,
                        purpose: Some(ExecutionPurpose::Operation),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "script-extended-sync-wait").await;
    assert_eq!(request.kind, "start_script_job");
    update_script_job(
        &runtime,
        "script-extended-sync-wait",
        &request,
        "running",
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(10_250)).await;
    update_script_job(
        &runtime,
        "script-extended-sync-wait",
        &request,
        "completed",
        Some(ShellCommandExecutionState::Completed),
        Some(0),
        Some("done\n"),
        None,
        None,
    )
    .await;
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert!(result.output.get("execution_state").is_none());
    assert!(result.output.get("command_completed").is_none());
    assert!(result.output.get("job_id").is_none());
    assert!(result.output.get("promoted_to_job").is_none());
    assert!(runtime.runner_registry.list_jobs(Some(10)).await.is_empty());
    assert!(
        probe_patch_agent_request(&runtime, "script-extended-sync-wait")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn run_script_fast_missing_interpreter_retains_not_started_through_the_hidden_job() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime().with_structured_execution_sync_wait(Duration::from_millis(250));
    let project = register_script_job_agent(&runtime, "script-prestart-job", temp.path()).await;
    let auth = auth_context(None, true);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    script_call(
                        project,
                        None,
                        ShellScriptLanguage::Javascript,
                        "Promise.resolve();\n",
                    ),
                    Some(&auth),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "script-prestart-job").await;
    let queued = runtime
        .runner_registry
        .get_hidden_job_for_auth(
            Some(&crate::test_support::runner_access(&auth)),
            request.job_id.as_deref().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(queued.status, "agent_queued");
    assert_eq!(
        queued.started_at, None,
        "Runner request dispatch alone must not imply interpreter spawn"
    );
    update_script_job(
        &runtime,
        "script-prestart-job",
        &request,
        "failed",
        Some(ShellCommandExecutionState::NotStarted),
        None,
        None,
        None,
        Some(
            "interpreter_unavailable: JavaScript/Node interpreter is unavailable; command was not started",
        ),
    )
    .await;

    let result = task.await.unwrap();
    assert!(!result.success);
    assert_eq!(result.output["execution_state"], "not_started");
    assert_eq!(result.output["command_started"], false);
    assert_eq!(result.output["command_completed"], false);
    assert_eq!(result.output["promoted_to_job"], false);
    assert_eq!(result.output["terminal"], true);
    assert_eq!(result.output["failure_kind"], "interpreter_unavailable");
    assert!(runtime.runner_registry.list_jobs(Some(10)).await.is_empty());
}

#[tokio::test]
async fn run_script_slow_handoff_keeps_typed_payload_ephemeral_and_safe_metadata_durable() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime().with_structured_execution_sync_wait(Duration::from_millis(40));
    let project = register_script_job_agent(&runtime, "script-slow-job", temp.path()).await;
    let session = runtime.sessions.start_session_with_guards(
        Some(project.clone()),
        Some("structured script continuation".to_string()),
        SessionMode::Normal,
        sessions::SessionGuards::default(),
    );
    let auth = auth_context(None, true);
    let unique_body = format!(
        "// raw-body-{}\n{}\nconsole.log('done');\n",
        uuid::Uuid::new_v4(),
        "// typed script payload\n".repeat(1_800)
    );
    assert!(unique_body.len() > 32 * 1024);
    let unique_arg = format!("raw-arg-{}", uuid::Uuid::new_v4());
    let unique_stdin = format!("raw-stdin-{}", uuid::Uuid::new_v4());
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let auth = auth.clone();
        let body = unique_body.clone();
        let arg = unique_arg.clone();
        let stdin = unique_stdin.clone();
        let session_id = session.session_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::RunScript {
                        project,
                        language: ShellScriptLanguage::Javascript,
                        script: body,
                        args: vec![arg],
                        stdin: Some(stdin),
                        session_id: Some(session_id),
                        timeout_secs: Some(60),
                        sync_wait_secs: None,
                        cwd: None,
                        purpose: Some(ExecutionPurpose::Operation),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "script-slow-job").await;
    assert_eq!(request.kind, "start_script_job");
    assert_eq!(request.command, "");
    assert!(request.process.is_none());
    let payload = request.script.as_ref().expect("typed script payload");
    assert_eq!(payload.language, ShellScriptLanguage::Javascript);
    assert_eq!(payload.script, unique_body);
    assert_eq!(payload.args.as_slice(), std::slice::from_ref(&unique_arg));
    assert_eq!(request.stdin.as_deref(), Some(unique_stdin.as_str()));
    assert!(!request.command.contains(&unique_body));
    update_script_job(
        &runtime,
        "script-slow-job",
        &request,
        "running",
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    let handoff = task.await.unwrap();
    assert!(handoff.success, "{:?}", handoff.error);
    assert_eq!(handoff.output["promoted_to_job"], true);
    assert_eq!(handoff.output["execution_state"], "running");
    let job_id = handoff.output["job_id"].as_str().unwrap();
    assert_eq!(request.job_id.as_deref(), Some(job_id));

    let job = runtime.runner_registry.get_job(job_id).await.unwrap();
    assert_eq!(job.kind, "run_script");
    assert_eq!(job.session_id.as_deref(), Some(session.session_id.as_str()));
    let metadata = job
        .structured_execution
        .as_ref()
        .expect("safe structured metadata");
    assert_eq!(metadata.execution_source, "run_script");
    assert_eq!(metadata.language, Some(ShellScriptLanguage::Javascript));
    assert_eq!(metadata.script_bytes, Some(unique_body.len()));
    assert_eq!(metadata.arg_count, 1);
    assert!(metadata.stdin_present);
    let durable = serde_json::to_string(&job).unwrap();
    for raw in [&unique_body, &unique_arg, &unique_stdin] {
        assert!(
            !durable.contains(raw),
            "durable Job state leaked raw structured execution input"
        );
    }
    let handoff_summary = runtime
        .dispatch_with_auth(
            ToolCall::SessionHandoffSummary {
                session_id: session.session_id.clone(),
                project: Some(project.clone()),
                include_workspace: Some(false),
                include_checkpoints: Some(false),
                include_validation: Some(false),
                summary_only: false,
                limit: Some(20),
            },
            Some(&auth),
        )
        .await;
    assert!(handoff_summary.success, "{:?}", handoff_summary.error);
    assert_eq!(handoff_summary.output["jobs"]["active_count"], 1);
    assert_eq!(
        handoff_summary.output["jobs"]["recent"][0]["job_id"],
        job_id
    );
    let safe_session_projection = serde_json::to_string(&handoff_summary.output["jobs"]).unwrap();
    for raw in [&unique_body, &unique_arg, &unique_stdin] {
        assert!(
            !safe_session_projection.contains(raw),
            "Session Job projection leaked raw structured execution input"
        );
    }
    let observed = runtime
        .dispatch_with_auth(
            ToolCall::ObserveJobs {
                items: vec![ObserveJobsItem {
                    job_id: job_id.to_string(),
                    after_observation_token: None,
                }],
                tail_lines: 40,
                wait_secs: None,
            },
            Some(&auth),
        )
        .await;
    assert!(observed.success, "{:?}", observed.error);
    let observed_job = &observed.output["items"][0]["output"];
    assert_eq!(observed_job["status"], "running");
    assert!(observed_job["command_execution_state"].is_null());
    assert_eq!(
        observed_job["structured_execution"]["execution_source"],
        "run_script"
    );
    assert_eq!(
        observed_job["structured_execution"]["language"],
        "javascript"
    );
    assert_eq!(
        observed_job["structured_execution"]["script_bytes"],
        unique_body.len()
    );
    let observed_serialized = serde_json::to_string(&observed.output).unwrap();
    for raw in [&unique_body, &unique_arg, &unique_stdin] {
        assert!(
            !observed_serialized.contains(raw),
            "observe_jobs leaked raw structured script input"
        );
    }
    assert!(!observed_serialized.contains(".codex-inspect"));
    let session_summary = runtime
        .sessions
        .summary(&session.session_id, Some(100))
        .unwrap();
    assert_eq!(
        session_summary
            .events
            .iter()
            .filter(|event| event.tool_name == "run_script" && event.kind == "tool_call_started")
            .count(),
        1,
        "handoff must not record a fake second model tool execution"
    );

    update_script_job(
        &runtime,
        "script-slow-job",
        &request,
        "completed",
        Some(ShellCommandExecutionState::Completed),
        Some(0),
        Some("done\n"),
        None,
        None,
    )
    .await;
    let terminal = runtime.runner_registry.get_job(job_id).await.unwrap();
    assert_eq!(terminal.status, "completed");
    assert_eq!(
        terminal.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    assert!(probe_patch_agent_request(&runtime, "script-slow-job")
        .await
        .is_none());
}

#[tokio::test]
async fn typescript_slow_handoff_keeps_one_execution_and_safe_durable_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime().with_structured_execution_sync_wait(Duration::from_millis(40));
    let project = register_script_job_agent(&runtime, "typescript-slow-job", temp.path()).await;
    let session = runtime.sessions.start_session_with_guards(
        Some(project.clone()),
        Some("typescript structured script continuation".to_string()),
        SessionMode::Normal,
        sessions::SessionGuards::default(),
    );
    let unique_body = format!(
        "interface Secret {{ value: string }}\nconst secret: Secret = {{ value: 'raw-ts-body-{}' }};\nconsole.log(secret.value);\n",
        uuid::Uuid::new_v4()
    );
    let unique_arg = format!("raw-ts-arg-{}", uuid::Uuid::new_v4());
    let unique_stdin = format!("raw-ts-stdin-{}", uuid::Uuid::new_v4());
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let body = unique_body.clone();
        let arg = unique_arg.clone();
        let stdin = unique_stdin.clone();
        let session_id = session.session_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::RunScript {
                        project,
                        language: ShellScriptLanguage::Typescript,
                        script: body,
                        args: vec![arg],
                        stdin: Some(stdin),
                        session_id: Some(session_id),
                        timeout_secs: Some(60),
                        sync_wait_secs: None,
                        cwd: None,
                        purpose: Some(ExecutionPurpose::Operation),
                    },
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });

    let request = wait_for_patch_agent_request(&runtime, "typescript-slow-job").await;
    assert_eq!(request.kind, "start_script_job");
    assert_eq!(request.command, "");
    assert!(request.process.is_none());
    let payload = request.script.as_ref().expect("typed TypeScript payload");
    assert_eq!(payload.language, ShellScriptLanguage::Typescript);
    assert_eq!(payload.script, unique_body);
    assert_eq!(payload.args.as_slice(), std::slice::from_ref(&unique_arg));
    assert_eq!(request.stdin.as_deref(), Some(unique_stdin.as_str()));
    update_script_job(
        &runtime,
        "typescript-slow-job",
        &request,
        "running",
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    let handoff = task.await.unwrap();
    assert!(handoff.success, "{:?}", handoff.error);
    assert_eq!(handoff.output["promoted_to_job"], true);
    assert_eq!(handoff.output["execution_state"], "running");
    let job_id = handoff.output["job_id"].as_str().unwrap();
    assert_eq!(request.job_id.as_deref(), Some(job_id));
    let job = runtime.runner_registry.get_job(job_id).await.unwrap();
    assert_eq!(job.kind, "run_script");
    let metadata = job
        .structured_execution
        .as_ref()
        .expect("safe TypeScript structured metadata");
    assert_eq!(metadata.execution_source, "run_script");
    assert_eq!(metadata.language, Some(ShellScriptLanguage::Typescript));
    assert_eq!(metadata.script_bytes, Some(unique_body.len()));
    assert_eq!(metadata.arg_count, 1);
    assert!(metadata.stdin_present);
    let durable = serde_json::to_string(&job).unwrap();
    for raw in [&unique_body, &unique_arg, &unique_stdin] {
        assert!(
            !durable.contains(raw),
            "durable Job leaked raw TypeScript input"
        );
    }
    let session_summary = runtime
        .sessions
        .summary(&session.session_id, Some(100))
        .unwrap();
    assert_eq!(
        session_summary
            .events
            .iter()
            .filter(|event| event.tool_name == "run_script" && event.kind == "tool_call_started")
            .count(),
        1,
        "TypeScript handoff must not record or launch a second model tool execution"
    );

    update_script_job(
        &runtime,
        "typescript-slow-job",
        &request,
        "completed",
        Some(ShellCommandExecutionState::Completed),
        Some(0),
        Some("done\n"),
        None,
        None,
    )
    .await;
    assert!(probe_patch_agent_request(&runtime, "typescript-slow-job")
        .await
        .is_none());
}

#[tokio::test]
async fn typescript_started_runtime_rejection_remains_completed_nonzero() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project =
        register_script_agent(&runtime, "typescript-runtime-reject", temp.path(), true).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    script_sync_call(
                        project,
                        None,
                        ShellScriptLanguage::Typescript,
                        "enum RuntimeSyntax { Value }\nconsole.log(RuntimeSyntax.Value);",
                    ),
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "typescript-runtime-reject").await;
    assert_eq!(
        request.script.as_ref().map(|script| script.language),
        Some(ShellScriptLanguage::Typescript)
    );
    complete_script_lifecycle(
        &runtime,
        "typescript-runtime-reject",
        request.request_id,
        ShellCommandExecutionState::Completed,
        Some(1),
        "",
        "ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX\n",
        Some("runtime rejected transform-required TypeScript syntax"),
    )
    .await;
    let result = task.await.unwrap();
    assert!(!result.success);
    assert_eq!(result.output["execution_state"], "completed");
    assert_eq!(result.output["command_started"], true);
    assert_eq!(result.output["command_completed"], true);
    assert_eq!(result.output["exit_code"], 1);
    assert_eq!(result.output["failure_kind"], "command_exit_nonzero");
}

#[tokio::test]
async fn run_script_nonzero_timeout_uncertainty_and_interpreter_absence_are_truthful() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project = register_script_agent(&runtime, "script-lifecycle", temp.path(), true).await;

    let nonzero_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    script_sync_call(
                        project,
                        None,
                        ShellScriptLanguage::Javascript,
                        "process.exit(19);",
                    ),
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "script-lifecycle").await;
    complete_script_lifecycle(
        &runtime,
        "script-lifecycle",
        request.request_id,
        ShellCommandExecutionState::Completed,
        Some(19),
        "",
        "",
        None,
    )
    .await;
    let nonzero = nonzero_task.await.unwrap();
    assert!(!nonzero.success);
    assert_eq!(nonzero.output["execution_state"], "completed");
    assert_eq!(nonzero.output["command_started"], true);
    assert_eq!(nonzero.output["command_completed"], true);
    assert_eq!(nonzero.output["exit_code"], 19);
    assert_eq!(nonzero.output["failure_kind"], "command_exit_nonzero");

    let timeout_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    script_sync_call(project, None, ShellScriptLanguage::Sh, "sleep 10"),
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "script-lifecycle").await;
    complete_script_lifecycle(
        &runtime,
        "script-lifecycle",
        request.request_id,
        ShellCommandExecutionState::TimedOut,
        Some(-1),
        "",
        "script timed out",
        Some("script timed out"),
    )
    .await;
    let timed_out = timeout_task.await.unwrap();
    assert_eq!(timed_out.output["execution_state"], "timed_out");
    assert_eq!(timed_out.output["command_started"], true);
    assert_eq!(timed_out.output["command_completed"], false);
    assert!(timed_out
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("do not blindly retry"));

    let uncertain_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    script_sync_call(project, None, ShellScriptLanguage::Sh, "true"),
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    wait_for_patch_agent_request(&runtime, "script-lifecycle").await;
    runtime
        .runner_registry
        .reconcile_disconnect("script-lifecycle", "inst")
        .await;
    let uncertain = uncertain_task.await.unwrap();
    assert_eq!(uncertain.output["execution_state"], "outcome_unknown");
    assert_eq!(uncertain.output["command_started"], true);
    assert_eq!(uncertain.output["command_completed"], false);
    assert!(uncertain
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("Do not automatically retry"));

    let missing_runtime = test_runtime();
    let missing_project =
        register_script_agent(&missing_runtime, "script-interpreter", temp.path(), true).await;
    let missing_task = tokio::spawn({
        let runtime = missing_runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    script_sync_call(
                        missing_project,
                        None,
                        ShellScriptLanguage::Powershell,
                        "Write-Output never",
                    ),
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&missing_runtime, "script-interpreter").await;
    complete_script_lifecycle(
        &missing_runtime,
        "script-interpreter",
        request.request_id,
        ShellCommandExecutionState::NotStarted,
        None,
        "",
        "",
        Some(
            "interpreter_unavailable: powershell interpreter is unavailable; command was not started",
        ),
    )
    .await;
    let missing = missing_task.await.unwrap();
    assert_eq!(missing.output["execution_state"], "not_started");
    assert_eq!(missing.output["command_started"], false);
    assert_eq!(missing.output["command_completed"], false);
    assert_eq!(missing.output["failure_kind"], "interpreter_unavailable");
}

#[derive(Default)]
struct CapturingActivity {
    commands: Mutex<Vec<Option<String>>>,
}

impl ActivityRecorder for CapturingActivity {
    fn record(&self, record: ActivityRecord<'_>) {
        self.commands
            .lock()
            .unwrap()
            .push(record.command.map(str::to_string));
    }
}

#[tokio::test]
async fn run_script_session_defaults_and_evidence_are_body_and_stdin_free() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let frontend = root.join("frontend");
    std::fs::create_dir_all(&frontend).unwrap();
    let recorder = Arc::new(CapturingActivity::default());
    let runtime = test_runtime().with_activity_recorder(recorder.clone());
    let project = register_script_agent(&runtime, "script-context", &root, true).await;
    let session = runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(project.clone()),
                Some("script context".to_string()),
                SessionMode::Normal,
                sessions::SessionGuards::default(),
            )
            .with_execution_context(sessions::SessionExecutionContext {
                default_cwd: Some("frontend".to_string()),
                default_shell: Some(ExecutionShell::Bash),
                resource: None,
            }),
        )
        .unwrap();
    let raw_script = "console.log('RAW_SCRIPT_BODY');";
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    script_sync_call(
                        project,
                        Some(session_id),
                        ShellScriptLanguage::Javascript,
                        raw_script,
                    ),
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "script-context").await;
    assert_eq!(
        request.cwd.as_deref(),
        Some(frontend.to_string_lossy().as_ref())
    );
    assert_eq!(
        request.script.as_ref().unwrap().language,
        ShellScriptLanguage::Javascript,
        "Session default_shell must not override explicit language"
    );
    complete_script_lifecycle(
        &runtime,
        "script-context",
        request.request_id,
        ShellCommandExecutionState::Completed,
        Some(0),
        "",
        "",
        None,
    )
    .await;
    let result = task.await.unwrap();
    assert!(result.success);
    assert!(result.output.get("cwd").is_none());

    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    let event = finished_event(&summary, "run_script");
    assert_eq!(event.risk_class, "job_run");
    assert!(event.shell_like);
    let started = summary
        .events
        .iter()
        .find(|event| event.kind == "tool_call_started" && event.tool_name == "run_script")
        .unwrap();
    let input = started.input_summary.as_ref().unwrap();
    assert_eq!(input["language"], "javascript");
    assert_eq!(input["script_bytes"], raw_script.len());
    assert_eq!(input["arg_count"], 4);
    assert_eq!(input["stdin_present"], true);
    assert!(input.get("script").is_none());
    assert!(input.get("args").is_none());
    assert!(input.get("stdin").is_none());
    let serialized = serde_json::to_string(&summary).unwrap();
    assert!(!serialized.contains("RAW_SCRIPT_BODY"));
    assert!(!serialized.contains("independent stdin"));
    assert!(!serialized.contains("$(literal)"));

    let commands = recorder.commands.lock().unwrap();
    assert_eq!(commands.len(), 1);
    let expected_preview = format!("javascript script ({} bytes, 4 args)", raw_script.len());
    assert_eq!(commands[0].as_deref(), Some(expected_preview.as_str()));
    assert!(!commands[0].as_deref().unwrap().contains("RAW_SCRIPT_BODY"));
}

#[tokio::test]
async fn run_script_ssh_read_only_and_closed_session_boundaries_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let runtime = test_runtime();
    let project = register_script_agent(&runtime, "script-guards", &root, true).await;
    let other_root = temp.path().join("other-project");
    std::fs::create_dir_all(&other_root).unwrap();
    let other_project =
        register_script_agent(&runtime, "script-guards-other", &other_root, true).await;

    let ssh = runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(project.clone()),
                Some("remote script".to_string()),
                SessionMode::Normal,
                sessions::SessionGuards::default(),
            )
            .with_execution_context(sessions::SessionExecutionContext {
                default_cwd: Some("/srv/app".to_string()),
                default_shell: None,
                resource: Some("production".to_string()),
            }),
        )
        .unwrap();
    let unsupported = runtime
        .dispatch_with_auth(
            script_call(
                project.clone(),
                Some(ssh.session_id),
                ShellScriptLanguage::Sh,
                "touch marker",
            ),
            Some(&auth_context(None, true)),
        )
        .await;
    assert_eq!(unsupported.output["execution_state"], "not_started");
    assert_eq!(unsupported.output["command_started"], false);
    assert_eq!(unsupported.output["command_completed"], false);
    assert_eq!(unsupported.output["failure_kind"], "unsupported_resource");
    assert_eq!(unsupported.output["error_kind"], "unsupported_resource");
    assert_eq!(unsupported.output["recovery_kind"], "fix_input");
    assert!(unsupported.output.get("recovery_tool").is_none());
    assert!(probe_patch_agent_request(&runtime, "script-guards")
        .await
        .is_none());

    let mismatch = runtime
        .sessions
        .start_session(Some(other_project), Some("mismatched script".to_string()));
    let mismatched = runtime
        .dispatch_with_auth(
            script_call(
                project.clone(),
                Some(mismatch.session_id),
                ShellScriptLanguage::Sh,
                "touch marker",
            ),
            Some(&auth_context(None, true)),
        )
        .await;
    assert_eq!(
        mismatched.output["failure_kind"],
        "session_project_mismatch"
    );
    assert_eq!(mismatched.output["execution_state"], "not_started");
    assert_eq!(mismatched.output["command_started"], false);
    assert_eq!(mismatched.output["command_completed"], false);
    assert!(probe_patch_agent_request(&runtime, "script-guards")
        .await
        .is_none());

    let read_only = runtime.sessions.start_session_with_guards(
        Some(project.clone()),
        Some("read-only script".to_string()),
        SessionMode::ReadOnly,
        sessions::SessionGuards::default(),
    );
    let denied = runtime
        .dispatch_with_auth(
            script_call(
                project.clone(),
                Some(read_only.session_id),
                ShellScriptLanguage::Sh,
                "touch marker",
            ),
            Some(&auth_context(None, true)),
        )
        .await;
    assert_eq!(denied.output["guard"], "deny_shell_tools");
    assert_eq!(denied.output["execution_state"], "not_started");
    assert_eq!(denied.output["command_started"], false);
    assert_eq!(denied.output["command_completed"], false);
    assert_eq!(denied.output["failure_kind"], "session_guard_denied");

    let closed = runtime.sessions.start_session_with_guards(
        Some(project.clone()),
        Some("closed script".to_string()),
        SessionMode::Normal,
        sessions::SessionGuards::default(),
    );
    runtime.sessions.close_session(&closed.session_id).unwrap();
    let closed_result = runtime
        .dispatch_with_auth(
            script_call(
                project.clone(),
                Some(closed.session_id),
                ShellScriptLanguage::Sh,
                "true",
            ),
            Some(&auth_context(None, true)),
        )
        .await;
    assert_eq!(closed_result.output["execution_state"], "not_started");
    assert_eq!(closed_result.output["command_started"], false);
    assert_eq!(closed_result.output["command_completed"], false);
    assert_eq!(closed_result.output["failure_kind"], "session_closed");
}

#[tokio::test]
async fn run_script_shared_bounds_reject_before_enqueue_with_full_prestart_tuple() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project = register_script_agent(&runtime, "script-bounds", temp.path(), true).await;
    let invalid_calls = [
        ToolCall::RunScript {
            project: project.clone(),
            language: ShellScriptLanguage::Sh,
            script: String::new(),
            args: Vec::new(),
            stdin: None,
            session_id: None,
            timeout_secs: Some(60),
            sync_wait_secs: None,
            cwd: None,
            purpose: None,
        },
        ToolCall::RunScript {
            project: project.clone(),
            language: ShellScriptLanguage::Sh,
            script: "x".repeat(SCRIPT_MAX_BYTES + 1),
            args: Vec::new(),
            stdin: None,
            session_id: None,
            timeout_secs: Some(60),
            sync_wait_secs: None,
            cwd: None,
            purpose: None,
        },
        ToolCall::RunScript {
            project: project.clone(),
            language: ShellScriptLanguage::Sh,
            script: "bad\0script".to_string(),
            args: Vec::new(),
            stdin: None,
            session_id: None,
            timeout_secs: Some(60),
            sync_wait_secs: None,
            cwd: None,
            purpose: None,
        },
        ToolCall::RunScript {
            project: project.clone(),
            language: ShellScriptLanguage::Sh,
            script: "true".to_string(),
            args: vec![String::new(); 257],
            stdin: None,
            session_id: None,
            timeout_secs: Some(60),
            sync_wait_secs: None,
            cwd: None,
            purpose: None,
        },
        ToolCall::RunScript {
            project: project.clone(),
            language: ShellScriptLanguage::Sh,
            script: "true".to_string(),
            args: vec!["x".repeat(8_193)],
            stdin: None,
            session_id: None,
            timeout_secs: Some(60),
            sync_wait_secs: None,
            cwd: None,
            purpose: None,
        },
        ToolCall::RunScript {
            project: project.clone(),
            language: ShellScriptLanguage::Sh,
            script: "true".to_string(),
            args: Vec::new(),
            stdin: Some("x".repeat(65_537)),
            session_id: None,
            timeout_secs: Some(60),
            sync_wait_secs: None,
            cwd: None,
            purpose: None,
        },
        ToolCall::RunScript {
            project: project.clone(),
            language: ShellScriptLanguage::Sh,
            script: "true".to_string(),
            args: Vec::new(),
            stdin: None,
            session_id: None,
            timeout_secs: Some(60),
            sync_wait_secs: None,
            cwd: Some("bad\0cwd".to_string()),
            purpose: None,
        },
        ToolCall::RunScript {
            project: project.clone(),
            language: ShellScriptLanguage::Sh,
            script: "true".to_string(),
            args: Vec::new(),
            stdin: None,
            session_id: None,
            timeout_secs: Some(60),
            sync_wait_secs: Some(0),
            cwd: None,
            purpose: None,
        },
        ToolCall::RunScript {
            project: project.clone(),
            language: ShellScriptLanguage::Sh,
            script: "true".to_string(),
            args: Vec::new(),
            stdin: None,
            session_id: None,
            timeout_secs: Some(60),
            sync_wait_secs: Some(61),
            cwd: None,
            purpose: None,
        },
        ToolCall::RunScript {
            project: project.clone(),
            language: ShellScriptLanguage::Sh,
            script: "true".to_string(),
            args: Vec::new(),
            stdin: None,
            session_id: None,
            timeout_secs: Some(5),
            sync_wait_secs: Some(6),
            cwd: None,
            purpose: None,
        },
    ];
    for call in invalid_calls {
        let result = runtime
            .dispatch_with_auth(call, Some(&auth_context(None, true)))
            .await;
        assert_eq!(result.output["execution_state"], "not_started");
        assert_eq!(result.output["command_started"], false);
        assert_eq!(result.output["command_completed"], false);
        assert_eq!(result.output["failure_kind"], "invalid_arguments");
    }
}

#[tokio::test]
async fn model_facing_run_script_session_denials_keep_phase_a_tuple() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project = register_script_agent(&runtime, "script-kernel", temp.path(), true).await;
    let read_only = runtime.sessions.start_session_with_guards(
        Some(project.clone()),
        Some("read-only model script".to_string()),
        SessionMode::ReadOnly,
        sessions::SessionGuards::default(),
    );
    let auth = auth_context(None, true);
    let outcome = runtime
        .call_tool_with_context(
            ToolCallRequest {
                tool_name: "run_script".to_string(),
                arguments: json!({
                    "project": project,
                    "language": "sh",
                    "script": "true",
                    "session_id": read_only.session_id
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
    let result = outcome.result.unwrap();
    assert_eq!(result.output["execution_state"], "not_started");
    assert_eq!(result.output["command_started"], false);
    assert_eq!(result.output["command_completed"], false);
    assert_eq!(result.output["failure_kind"], "session_guard_denied");
    assert!(probe_patch_agent_request(&runtime, "script-kernel")
        .await
        .is_none());
}
