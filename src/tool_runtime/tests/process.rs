//! Model-facing structured process execution contract.

use super::super::*;
use super::support::*;
use crate::runner_protocol::{
    RunnerCapabilities, RunnerJobUpdateRequest, RunnerResultPayload, RunnerResultRequest,
    ShellCommandExecutionState, ShellJobActivity, ShellJobActivityPhase, ShellJobActivitySource,
    ShellJobActivityState,
};
use crate::tool_runtime::kernel::{ToolCallContext, ToolCallRequest, ToolTransport};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

fn process_call(project: String, session_id: Option<String>) -> ToolCall {
    ToolCall::RunProcess {
        project,
        executable: "argv-helper".to_string(),
        args: vec![
            "two words".to_string(),
            "$(literal)".to_string(),
            "雪".to_string(),
        ],
        stdin: Some("input\n".to_string()),
        session_id,
        timeout_secs: Some(30),
        sync_wait_secs: None,
        cwd: None,
        purpose: Some(ExecutionPurpose::Diagnostic),
    }
}

fn process_sync_call(project: String, session_id: Option<String>) -> ToolCall {
    let mut call = process_call(project, session_id);
    let ToolCall::RunProcess { sync_wait_secs, .. } = &mut call else {
        unreachable!("process_call must return RunProcess");
    };
    *sync_wait_secs = Some(30);
    call
}

async fn register_process_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    root: &std::path::Path,
    structured_process_argv: bool,
) -> String {
    let capabilities = RunnerCapabilities {
        shell: true,
        structured_validation_argv: true,
        structured_process_argv,
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

async fn register_process_job_agent(
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

async fn register_detached_process_job_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    root: &std::path::Path,
    detached_process_jobs: bool,
) -> String {
    let capabilities = RunnerCapabilities {
        shell: true,
        async_jobs: true,
        async_shell_jobs: true,
        structured_validation_argv: true,
        structured_process_argv: true,
        structured_execution_jobs: true,
        detached_process_jobs,
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

fn detached_process_call_with(
    project: String,
    idempotency_key: &str,
    executable: &str,
    args: Vec<String>,
    stdin: Option<String>,
) -> ToolCall {
    ToolCall::RunDetachedProcess {
        project,
        idempotency_key: idempotency_key.to_string(),
        executable: executable.to_string(),
        args,
        stdin,
        session_id: None,
        timeout_secs: Some(30),
        cwd: None,
        purpose: Some(ExecutionPurpose::Operation),
    }
}

fn detached_process_call(project: String) -> ToolCall {
    detached_process_call_with(
        project,
        "detached-process-test-key",
        "argv-helper",
        vec!["detached".to_string()],
        None,
    )
}

async fn update_process_job(
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
    let activity = (state.is_none() && matches!(status, "running" | "stop_requested")).then_some(
        ShellJobActivity {
            state: ShellJobActivityState::Working,
            phase: ShellJobActivityPhase::ProcessRunning,
            source: ShellJobActivitySource::RunnerExecution,
        },
    );
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
            activity,
            finished: state.is_some(),
        })
        .await
        .unwrap();
}

async fn complete_process_lifecycle(
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
                duration_ms: Some(7),
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

async fn dispatch_process_until_request(
    runtime: &ToolRuntime,
    client_id: &str,
    call: ToolCall,
    auth: crate::auth::AuthContext,
) -> (
    tokio::task::JoinHandle<ToolResult>,
    crate::runner_protocol::RunnerRequest,
) {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.dispatch_with_auth(call, Some(&auth)).await }
    });
    let request = wait_for_patch_agent_request(runtime, client_id).await;
    (task, request)
}

#[tokio::test]
async fn run_process_enqueues_only_typed_argv_and_reports_completed_exit_codes() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project = register_process_agent(&runtime, "process-agent", temp.path(), true).await;
    let bootstrap = auth_context(None, true);

    for (exit_code, expected_success) in [(0, true), (19, false)] {
        let task = tokio::spawn({
            let runtime = runtime.clone();
            let project = project.clone();
            let bootstrap = bootstrap.clone();
            async move {
                runtime
                    .dispatch_with_auth(process_sync_call(project, None), Some(&bootstrap))
                    .await
            }
        });
        let request = wait_for_patch_agent_request(&runtime, "process-agent").await;
        assert_eq!(request.kind, "run_process");
        assert_eq!(request.command, "");
        let process = request.process.as_ref().expect("typed process payload");
        assert_eq!(process.executable, "argv-helper");
        assert_eq!(
            process.args,
            ["two words", "$(literal)", "雪"].map(str::to_string)
        );
        assert_eq!(request.stdin.as_deref(), Some("input\n"));

        complete_process_lifecycle(
            &runtime,
            "process-agent",
            request.request_id,
            ShellCommandExecutionState::Completed,
            Some(exit_code),
            "stdout",
            "stderr",
            None,
        )
        .await;
        let result = task.await.unwrap();
        assert_eq!(result.success, expected_success);
        if expected_success {
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
                "execution_source",
                "process_summary",
            ] {
                assert!(
                    result.output.get(omitted).is_none(),
                    "{omitted}: {}",
                    result.output
                );
            }
        } else {
            assert_eq!(result.output["execution_state"], "completed");
            assert_eq!(result.output["command_started"], true);
            assert_eq!(result.output["command_completed"], true);
            assert_eq!(result.output["exit_code"], exit_code);
            assert_eq!(result.output["execution_source"], "run_process");
            assert!(result.output["process_summary"].as_str().is_some());
            assert_eq!(result.output["purpose"], "diagnostic");
        }
    }
}

#[tokio::test]
async fn run_process_preserves_remote_windows_cwd_syntax_before_dispatch() {
    let runtime = test_runtime();
    let client_id = "process-windows-cwd";
    register_agent_with_projects(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            shell: true,
            structured_validation_argv: true,
            structured_process_argv: true,
            ..Default::default()
        },
        vec![registered_project("demo", r"\\?\E:\git\webcodex")],
    )
    .await;
    let project = crate::tool_runtime::runner_project_runtime_id(client_id, "demo");
    let mut call = process_sync_call(project, None);
    let ToolCall::RunProcess { cwd, .. } = &mut call else {
        unreachable!("process_sync_call must return RunProcess");
    };
    *cwd = Some("apps/桌面 project".to_string());

    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(call, Some(&auth_context(None, true)))
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, client_id).await;
    assert_eq!(request.kind, "run_process");
    assert_eq!(
        request.cwd.as_deref(),
        Some(r"\\?\E:\git\webcodex\apps\桌面 project")
    );
    assert_eq!(request.command, "");
    assert!(request.process.is_some());

    complete_process_lifecycle(
        &runtime,
        client_id,
        request.request_id,
        ShellCommandExecutionState::Completed,
        Some(0),
        "",
        "",
        None,
    )
    .await;
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
}

#[tokio::test]
async fn detached_process_requires_job_run_and_detach_scopes_before_any_admission() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let capabilities = RunnerCapabilities {
        shell: true,
        async_jobs: true,
        async_shell_jobs: true,
        structured_validation_argv: true,
        structured_process_argv: true,
        structured_execution_jobs: true,
        detached_process_jobs: true,
        ..Default::default()
    };
    register_agent_with_projects(
        &runtime,
        "detached-scope-gate",
        Some("alice"),
        capabilities,
        vec![registered_project("demo", &temp.path().to_string_lossy())],
    )
    .await;
    let project = crate::tool_runtime::runner_project_runtime_id("detached-scope-gate", "demo");

    for (label, mut auth, key) in [
        (
            "job-run-only",
            auth_context(Some("alice"), false),
            "scope-run",
        ),
        (
            "shared-key-default",
            shared_key_auth_context("detached-scope"),
            "scope-shared",
        ),
        ("open-default", open_auth_context(), "scope-open"),
        (
            "managed-oauth-default",
            managed_oauth_auth_context("alice", None),
            "scope-oauth-default",
        ),
    ] {
        if label == "job-run-only" {
            auth.scopes = vec![crate::auth::SCOPE_JOB_RUN.to_string()];
        }
        let outcome = runtime
            .call_tool_with_context(
                ToolCallRequest {
                    tool_name: "run_detached_process".to_string(),
                    arguments: json!({
                        "project": project,
                        "idempotency_key": key,
                        "executable": "argv-helper",
                        "args": ["detached"]
                    }),
                },
                ToolCallContext {
                    transport: ToolTransport::Api,
                    session_id: None,
                    auth: Some(&auth),
                    window: None,
                    record_oauth_scope_denials: true,
                    host_file_import_trust:
                        crate::tool_runtime::kernel::HostFileImportTrust::Untrusted,
                },
            )
            .await;
        assert!(
            !outcome.success,
            "{label} unexpectedly admitted detached execution"
        );
        assert!(
            outcome.error_status.is_some(),
            "{label} should fail at scope gate"
        );
        assert!(runtime.runner_registry.list_jobs(Some(10)).await.is_empty());
        assert!(probe_patch_agent_request(&runtime, "detached-scope-gate")
            .await
            .is_none());
    }

    let mut detach_only = auth_context(Some("alice"), false);
    detach_only.scopes = vec![crate::auth::SCOPE_JOB_DETACH.to_string()];
    let denied = runtime
        .call_tool_with_context(
            ToolCallRequest {
                tool_name: "run_detached_process".to_string(),
                arguments: json!({
                    "project": project,
                    "idempotency_key": "scope-detach-only",
                    "executable": "argv-helper",
                    "args": ["detached"]
                }),
            },
            ToolCallContext {
                transport: ToolTransport::Api,
                session_id: None,
                auth: Some(&detach_only),
                window: None,
                record_oauth_scope_denials: true,
                host_file_import_trust: crate::tool_runtime::kernel::HostFileImportTrust::Untrusted,
            },
        )
        .await;
    assert!(!denied.success);
    assert!(denied.error_status.is_some());
    assert!(runtime.runner_registry.list_jobs(Some(10)).await.is_empty());
    assert!(probe_patch_agent_request(&runtime, "detached-scope-gate")
        .await
        .is_none());

    let mut both = auth_context(Some("alice"), false);
    both.scopes = vec![
        crate::auth::SCOPE_JOB_RUN.to_string(),
        crate::auth::SCOPE_JOB_DETACH.to_string(),
    ];
    let admitted = runtime
        .call_tool_with_context(
            ToolCallRequest {
                tool_name: "run_detached_process".to_string(),
                arguments: json!({
                    "project": project,
                    "idempotency_key": "scope-both",
                    "executable": "argv-helper",
                    "args": ["detached"]
                }),
            },
            ToolCallContext {
                transport: ToolTransport::Api,
                session_id: None,
                auth: Some(&both),
                window: None,
                record_oauth_scope_denials: true,
                host_file_import_trust: crate::tool_runtime::kernel::HostFileImportTrust::Untrusted,
            },
        )
        .await;
    assert!(
        admitted.success,
        "both scopes should reach admission: {admitted:?}"
    );
    let result = admitted.result.expect("admitted detached tool result");
    assert!(result.success, "{:?}", result.error);
    assert_eq!(runtime.runner_registry.list_jobs(Some(10)).await.len(), 1);
    assert_eq!(
        wait_for_patch_agent_request(&runtime, "detached-scope-gate")
            .await
            .kind,
        "start_detached_process_job"
    );
    assert!(probe_patch_agent_request(&runtime, "detached-scope-gate")
        .await
        .is_none());
}

#[test]
fn detached_process_audit_is_body_free_for_arbitrary_sentinel_and_digest() {
    let sentinel = "wc-detached-private-sentinel-4d7a9e";
    let digest = format!("{:x}", Sha256::digest(sentinel.as_bytes()));
    let raw = json!({
        "project": "agent:test:demo",
        "idempotency_key": "privacy-key",
        "executable": format!("/tmp/{sentinel}"),
        "args": [format!("arg-{sentinel}")],
        "stdin": format!("stdin-{sentinel}"),
        "timeout_secs": 30,
        "purpose": "operation"
    });
    let log = super::super::tool_audit::session_log_arguments_for_tool_request(
        "run_detached_process",
        &raw,
    );
    let retained =
        super::super::sessions::session_input_summary_for_tool("run_detached_process", &raw);
    let text = format!("{}\n{}", log, retained);
    assert!(
        !text.contains(sentinel),
        "detached audit leaked raw body: {text}"
    );
    assert!(
        !text.contains(&digest),
        "detached audit leaked body digest: {text}"
    );
    assert!(
        !text.contains("privacy-key"),
        "detached audit leaked idempotency key: {text}"
    );
    assert_eq!(log["process_summary"], "detached process (1 args)");
}

#[tokio::test]
async fn detached_process_idempotency_replays_same_intent_and_rejects_conflict() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project =
        register_detached_process_job_agent(&runtime, "detached-idempotency", temp.path(), true)
            .await;
    let auth = auth_context(None, true);
    let key = "same-public-initiation";
    let first = runtime
        .dispatch_with_auth(
            detached_process_call_with(
                project.clone(),
                key,
                "argv-helper",
                vec!["detached".to_string()],
                None,
            ),
            Some(&auth),
        )
        .await;
    assert!(first.success, "{:?}", first.error);
    let job_id = first.output["job_id"].as_str().unwrap().to_string();
    let request = wait_for_patch_agent_request(&runtime, "detached-idempotency").await;
    assert_eq!(request.job_id.as_deref(), Some(job_id.as_str()));

    let replay = runtime
        .dispatch_with_auth(
            detached_process_call_with(
                project.clone(),
                key,
                "argv-helper",
                vec!["detached".to_string()],
                None,
            ),
            Some(&auth),
        )
        .await;
    assert!(replay.success, "{:?}", replay.error);
    assert_eq!(replay.output["job_id"], job_id);
    assert!(
        probe_patch_agent_request(&runtime, "detached-idempotency")
            .await
            .is_none(),
        "same-key same-intent replay must not redispatch"
    );
    assert_eq!(runtime.runner_registry.list_jobs(Some(10)).await.len(), 1);

    let conflict = runtime
        .dispatch_with_auth(
            detached_process_call_with(
                project,
                key,
                "argv-helper",
                vec!["different-intent".to_string()],
                None,
            ),
            Some(&auth),
        )
        .await;
    assert!(!conflict.success);
    assert_eq!(conflict.output["failure_kind"], "idempotency_conflict");
    assert_eq!(conflict.output["execution_state"], "not_started");
    assert!(
        probe_patch_agent_request(&runtime, "detached-idempotency")
            .await
            .is_none(),
        "conflicting key must fail before redispatch"
    );
    assert_eq!(runtime.runner_registry.list_jobs(Some(10)).await.len(), 1);
}

#[tokio::test]
async fn detached_process_lost_initiation_after_server_restart_recovers_same_job_without_redispatch(
) {
    let temp = tempfile::tempdir().unwrap();
    let first_runtime = test_runtime();
    let project = register_detached_process_job_agent(
        &first_runtime,
        "detached-restart-recovery",
        temp.path(),
        true,
    )
    .await;
    let auth = auth_context(None, true);
    let key = "lost-response-restart-key";
    let first = first_runtime
        .dispatch_with_auth(
            detached_process_call_with(
                project,
                key,
                "argv-helper",
                vec!["detached".to_string()],
                None,
            ),
            Some(&auth),
        )
        .await;
    assert!(first.success, "{:?}", first.error);
    let job_id = first.output["job_id"].as_str().unwrap().to_string();
    let request = wait_for_patch_agent_request(&first_runtime, "detached-restart-recovery").await;
    let context = request
        .job_context
        .clone()
        .expect("durable detached context");
    assert_eq!(context.command_preview, "detached process (1 args)");
    assert_eq!(request.job_id.as_deref(), Some(job_id.as_str()));

    let restarted = test_runtime();
    let capabilities = RunnerCapabilities {
        shell: true,
        async_jobs: true,
        async_shell_jobs: true,
        structured_validation_argv: true,
        structured_process_argv: true,
        structured_execution_jobs: true,
        detached_process_jobs: true,
        job_state_reconciliation: true,
        coding_agent_runs: false,
        ..Default::default()
    };
    restarted
        .runner_registry
        .register(crate::runner_protocol::RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: Some(crate::runner_protocol::ShellJobInventory {
                active_complete: true,
                jobs: vec![crate::runner_protocol::ShellJobSnapshot {
                    job_id: job_id.clone(),
                    request_id: request.request_id.clone(),
                    status: "running".to_string(),
                    update_seq: 1,
                    created_at: request.created_at,
                    started_at: Some(request.created_at),
                    ended_at: None,
                    exit_code: None,
                    duration_ms: None,
                    error: None,
                    command_execution_state: None,
                    context,
                    stdout: Default::default(),
                    stderr: Default::default(),
                    validation_progress: None,
                    activity: Some(ShellJobActivity {
                        state: ShellJobActivityState::Working,
                        phase: ShellJobActivityPhase::ProcessRunning,
                        source: ShellJobActivitySource::RunnerExecution,
                    }),
                }],
            }),
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "detached-restart-recovery".to_string(),
            runner_instance_id: "inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: None,
            hostname: None,
            host_context: None,
            capabilities: crate::test_support::current_runner_capabilities(capabilities),
            policy: None,
        })
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &restarted.runner_registry,
        "detached-restart-recovery",
        "inst",
        vec![registered_project("demo", &temp.path().to_string_lossy())],
    )
    .await;
    let restarted_project =
        crate::tool_runtime::runner_project_runtime_id("detached-restart-recovery", "demo");
    let retry = restarted
        .dispatch_with_auth(
            detached_process_call_with(
                restarted_project,
                key,
                "argv-helper",
                vec!["detached".to_string()],
                None,
            ),
            Some(&auth),
        )
        .await;
    assert!(
        !retry.success,
        "restart replay must require observation, not redispatch"
    );
    assert_eq!(
        retry.output["failure_kind"],
        "idempotency_recovery_required"
    );
    assert_eq!(retry.output["job_id"], job_id);
    assert_eq!(retry.output["redispatched"], false);
    assert!(
        probe_patch_agent_request(&restarted, "detached-restart-recovery")
            .await
            .is_none(),
        "lost-response recovery must not enqueue a second payload"
    );
    let status = restarted
        .job_status_for_auth(job_id.clone(), false, Some(&auth))
        .await;
    assert!(
        status.success,
        "reconstructed logical Job must remain observable"
    );
    assert_eq!(status.output["job_id"], job_id);
    assert_eq!(status.output["status"], "running");
}

#[tokio::test]
async fn detached_process_requires_explicit_runner_authority_before_admission() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project =
        register_detached_process_job_agent(&runtime, "detached-no-authority", temp.path(), false)
            .await;
    let auth = auth_context(None, true);
    let result = runtime
        .dispatch_with_auth(detached_process_call(project), Some(&auth))
        .await;
    assert!(!result.success);
    assert_eq!(result.output["execution_state"], "not_started");
    assert_eq!(result.output["failure_kind"], "capability_unavailable");
    assert!(probe_patch_agent_request(&runtime, "detached-no-authority")
        .await
        .is_none());
}

#[tokio::test]
async fn detached_process_uses_existing_job_identity_and_typed_runner_request() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project =
        register_detached_process_job_agent(&runtime, "detached-product-path", temp.path(), true)
            .await;
    let auth = auth_context(None, true);
    let result = runtime
        .dispatch_with_auth(detached_process_call(project), Some(&auth))
        .await;
    assert!(result.success, "{:?}", result.error);
    let job_id = result.output["job_id"].as_str().unwrap().to_string();
    assert_eq!(result.output["execution_source"], "run_detached_process");

    let request = wait_for_patch_agent_request(&runtime, "detached-product-path").await;
    assert_eq!(request.kind, "start_detached_process_job");
    assert_eq!(request.job_id.as_deref(), Some(job_id.as_str()));
    assert!(request.process.is_some());
    assert!(request.script.is_none());
    assert!(probe_patch_agent_request(&runtime, "detached-product-path")
        .await
        .is_none());
}

#[tokio::test]
async fn run_process_fast_terminal_jobs_project_back_without_visible_duplicates() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime().with_structured_execution_sync_wait(Duration::from_millis(250));
    let project = register_process_job_agent(&runtime, "process-fast-job", temp.path()).await;
    let auth = auth_context(None, true);

    for (exit_code, status, expected_success) in [(0, "completed", true), (19, "failed", false)] {
        let (task, request) = dispatch_process_until_request(
            &runtime,
            "process-fast-job",
            process_call(project.clone(), None),
            auth.clone(),
        )
        .await;
        assert_eq!(request.kind, "start_process_job");
        assert_eq!(request.command, "");
        assert!(request.process.is_some());
        assert!(request.script.is_none());
        update_process_job(
            &runtime,
            "process-fast-job",
            &request,
            "running",
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        update_process_job(
            &runtime,
            "process-fast-job",
            &request,
            status,
            Some(ShellCommandExecutionState::Completed),
            Some(exit_code),
            Some("terminal stdout\n"),
            Some("terminal stderr\n"),
            None,
        )
        .await;

        let result = task.await.unwrap();
        assert_eq!(result.success, expected_success);
        if expected_success {
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
                "stdout_truncated",
                "stderr_truncated",
            ] {
                assert!(
                    result.output.get(omitted).is_none(),
                    "boring terminal success field {omitted} should be omitted: {}",
                    result.output
                );
            }
        } else {
            assert_eq!(result.output["execution_state"], "completed");
            assert_eq!(result.output["command_started"], true);
            assert_eq!(result.output["command_completed"], true);
            assert_eq!(result.output["exit_code"], exit_code);
            assert_eq!(result.output["promoted_to_job"], false);
            assert_eq!(result.output["terminal"], true);
            assert!(result.output["job_id"].is_null());
            assert!(result.output["job_status"].is_null());
            assert_eq!(result.output["async_handoff_available"], true);
            assert_eq!(result.output["failure_kind"], "command_exit_nonzero");
            assert_eq!(result.output["tool_failure"], false);
        }
        assert!(
            runtime
                .runner_registry
                .hidden_job_ids_for_test()
                .await
                .is_empty(),
            "a fast terminal execution must not retain its internal Job"
        );
        assert!(
            runtime.runner_registry.list_jobs(Some(10)).await.is_empty(),
            "a fast terminal execution must not become a public duplicate"
        );
    }
}

#[tokio::test]
async fn run_process_terminal_success_is_sparse_after_full_session_effect_recording() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime().with_structured_execution_sync_wait(Duration::from_millis(250));
    let project = register_process_job_agent(&runtime, "process-sparse-ledger", temp.path()).await;
    let session = runtime.sessions.start_session(Some(project.clone()), None);
    let session_id = session.session_id.clone();
    let auth = auth_context(None, true);

    let (task, request) = dispatch_process_until_request(
        &runtime,
        "process-sparse-ledger",
        process_call(project.clone(), Some(session_id.clone())),
        auth.clone(),
    )
    .await;
    update_process_job(
        &runtime,
        "process-sparse-ledger",
        &request,
        "running",
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    update_process_job(
        &runtime,
        "process-sparse-ledger",
        &request,
        "completed",
        Some(ShellCommandExecutionState::Completed),
        Some(0),
        None,
        None,
        None,
    )
    .await;

    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
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
        "effective_timeout_secs",
        "sync_wait_secs",
        "async_handoff_available",
        "failure_kind",
        "tool_failure",
        "stdout_tail",
        "stderr_tail",
        "stdout_lines",
        "stderr_lines",
        "stdout_truncated",
        "stderr_truncated",
        "process_summary",
        "execution_source",
    ] {
        assert!(
            result.output.get(omitted).is_none(),
            "model-facing boring success field {omitted} should be omitted: {}",
            result.output
        );
    }

    let sparse_bytes = serde_json::to_vec(&result.output).unwrap().len();
    assert!(
        sparse_bytes <= 420,
        "boring run_process success regressed above the model-facing context budget: {sparse_bytes} bytes"
    );
    eprintln!("run_process_sparse_terminal_success_bytes={sparse_bytes}");

    let summary = runtime.sessions.summary(&session_id, Some(20)).unwrap();
    let finished = summary
        .events
        .iter()
        .rev()
        .find(|event| event.kind == "tool_call_finished" && event.tool_name == "run_process")
        .expect("recorded run_process completion");
    assert_eq!(finished.exit_code, Some(0));
    let effect = finished
        .effect_evidence
        .as_ref()
        .expect("full lifecycle evidence must be recorded before model sparsification");
    assert_eq!(effect.execution_state.as_deref(), Some("completed"));
    assert_eq!(effect.command_started, Some(true));
    assert_eq!(effect.command_completed, Some(true));

    let schema = crate::tool_runtime::registry::output_schema_for_tool("run_process");
    let instance = json!({
        "success": true,
        "output": result.output,
        "error": null,
    });
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&instance, &schema)
        .unwrap_or_else(|error| {
            panic!("sparse terminal success must match run_process schema: {error}")
        });
}

#[tokio::test]
async fn run_process_fast_terminal_projection_does_not_silently_drop_retained_lines() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime().with_structured_execution_sync_wait(Duration::from_millis(250));
    let project = register_process_job_agent(&runtime, "process-fast-log-job", temp.path()).await;
    let auth = auth_context(None, true);
    let retained_stdout = (0..300)
        .map(|line| format!("retained-line-{line:03}\n"))
        .collect::<String>();
    assert!(
        retained_stdout.len() < crate::runner_protocol::JOB_SNAPSHOT_STREAM_MAX_BYTES,
        "fixture must fit the Runner-retained snapshot bound"
    );
    assert!(
        retained_stdout.chars().count() < super::super::helpers::COMMAND_STDIO_TAIL_CHARS,
        "fixture must fit the model-facing terminal projection bound"
    );

    let (task, request) = dispatch_process_until_request(
        &runtime,
        "process-fast-log-job",
        process_call(project.clone(), None),
        auth.clone(),
    )
    .await;
    update_process_job(
        &runtime,
        "process-fast-log-job",
        &request,
        "running",
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    update_process_job(
        &runtime,
        "process-fast-log-job",
        &request,
        "completed",
        Some(ShellCommandExecutionState::Completed),
        Some(0),
        Some(retained_stdout.as_str()),
        None,
        None,
    )
    .await;

    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        result.output["stdout_tail"].as_str(),
        Some(retained_stdout.as_str())
    );
    assert_eq!(result.output["stdout_lines"], 300);
    assert!(result.output.get("stdout_truncated").is_none());
    assert!(result.output.get("promoted_to_job").is_none());
    assert!(runtime.runner_registry.list_jobs(Some(10)).await.is_empty());
}

#[tokio::test]
async fn run_process_fast_prestart_rejection_retains_not_started_through_the_hidden_job() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime().with_structured_execution_sync_wait(Duration::from_millis(250));
    let project = register_process_job_agent(&runtime, "process-prestart-job", temp.path()).await;
    let auth = auth_context(None, true);
    let (task, request) = dispatch_process_until_request(
        &runtime,
        "process-prestart-job",
        process_call(project.clone(), None),
        auth.clone(),
    )
    .await;
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
        "Runner request dispatch alone must not imply child spawn"
    );
    update_process_job(
        &runtime,
        "process-prestart-job",
        &request,
        "failed",
        Some(ShellCommandExecutionState::NotStarted),
        None,
        None,
        None,
        Some("failed to spawn structured process: executable not found"),
    )
    .await;

    let result = task.await.unwrap();
    assert!(!result.success);
    assert_eq!(result.output["execution_state"], "not_started");
    assert_eq!(result.output["command_started"], false);
    assert_eq!(result.output["command_completed"], false);
    assert_eq!(result.output["promoted_to_job"], false);
    assert_eq!(result.output["terminal"], true);
    assert_eq!(result.output["failure_kind"], "spawn_failed");
    assert!(runtime.runner_registry.list_jobs(Some(10)).await.is_empty());
}

#[tokio::test]
async fn run_process_slow_handoff_is_queryable_once_and_keeps_the_original_budget() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime().with_structured_execution_sync_wait(Duration::from_millis(40));
    let project = register_process_job_agent(&runtime, "process-slow-job", temp.path()).await;
    let auth = auth_context(None, true);
    let started = Instant::now();
    let (task, request) = dispatch_process_until_request(
        &runtime,
        "process-slow-job",
        ToolCall::RunProcess {
            project: project.clone(),
            executable: "argv-helper".to_string(),
            args: vec![
                "two words".to_string(),
                "$(literal)".to_string(),
                "雪".to_string(),
            ],
            stdin: Some("input\n".to_string()),
            session_id: None,
            timeout_secs: Some(121),
            sync_wait_secs: Some(45),
            cwd: None,
            purpose: Some(ExecutionPurpose::Diagnostic),
        },
        auth.clone(),
    )
    .await;
    assert_eq!(request.kind, "start_process_job");
    assert_eq!(request.command, "");
    assert!(request.process.is_some());
    assert!(request.script.is_none());
    assert_eq!(request.timeout_secs, 121);
    update_process_job(
        &runtime,
        "process-slow-job",
        &request,
        "running",
        None,
        None,
        Some("progress already available\n"),
        None,
        None,
    )
    .await;

    let handoff = task.await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(handoff.success, "{:?}", handoff.error);
    assert_eq!(handoff.output["promoted_to_job"], true);
    assert_eq!(handoff.output["terminal"], false);
    assert_eq!(handoff.output["execution_state"], "running");
    assert_eq!(handoff.output["command_started"], true);
    assert_eq!(handoff.output["command_completed"], false);
    assert_eq!(handoff.output["effective_timeout_secs"], 121);
    assert_eq!(handoff.output["sync_wait_secs"], 45);
    assert_eq!(
        handoff.output["stdout_tail"],
        "progress already available\n"
    );
    assert_eq!(handoff.output["stdout_lines"], 1);
    assert_eq!(handoff.output["stdout_truncated"], false);
    assert_eq!(handoff.output["detected_summary"]["outcome"], "in_progress");
    assert_eq!(
        handoff.output["activity"],
        json!({
            "state": "working",
            "phase": "process_running",
            "source": "runner_execution"
        })
    );
    assert_eq!(
        handoff.output["detected_summary"]["progress"]["reason_code"],
        "process_running"
    );
    let job_id = handoff.output["job_id"].as_str().unwrap().to_string();
    assert_eq!(request.job_id.as_deref(), Some(job_id.as_str()));

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
    assert_eq!(status.output["activity"], handoff.output["activity"]);
    assert!(status.output["command_execution_state"].is_null());
    assert_eq!(
        status.output["structured_execution"]["execution_source"],
        "run_process"
    );
    assert_eq!(status.output["structured_execution"]["arg_count"], 3);
    assert_eq!(status.output["structured_execution"]["stdin_present"], true);
    let list = runtime
        .dispatch_with_auth(
            ToolCall::ListJobs {
                limit: Some(10),
                status: None,
                project: None,
                session_id: None,
            },
            Some(&auth),
        )
        .await;
    let listed = list.output["jobs"].as_array().unwrap();
    assert_eq!(
        listed.iter().filter(|job| job["job_id"] == job_id).count(),
        1
    );
    assert!(probe_patch_agent_request(&runtime, "process-slow-job")
        .await
        .is_none());

    update_process_job(
        &runtime,
        "process-slow-job",
        &request,
        "completed",
        Some(ShellCommandExecutionState::Completed),
        Some(0),
        Some("same execution complete\n"),
        None,
        None,
    )
    .await;
    let terminal = runtime
        .dispatch_with_auth(
            ToolCall::JobStatus {
                job_id: job_id.clone(),
                include_command_preview: false,
            },
            Some(&auth),
        )
        .await;
    assert!(terminal.success, "{:?}", terminal.error);
    assert_eq!(terminal.output["status"], "completed");
    assert!(terminal.output["activity"].is_null());
    assert_eq!(terminal.output["command_execution_state"], "completed");
    assert_eq!(
        terminal.output["structured_execution"]["execution_source"],
        "run_process"
    );
    assert!(probe_patch_agent_request(&runtime, "process-slow-job")
        .await
        .is_none());
}

#[tokio::test]
async fn run_process_sync_wait_validation_fails_before_execution_start() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project =
        register_process_job_agent(&runtime, "process-sync-wait-bounds", temp.path()).await;

    for (timeout_secs, sync_wait_secs) in [(60, 0), (60, 61), (5, 6)] {
        let result = runtime
            .dispatch_with_auth(
                ToolCall::RunProcess {
                    project: project.clone(),
                    executable: "argv-helper".to_string(),
                    args: Vec::new(),
                    stdin: None,
                    session_id: None,
                    timeout_secs: Some(timeout_secs),
                    sync_wait_secs: Some(sync_wait_secs),
                    cwd: None,
                    purpose: None,
                },
                Some(&auth_context(None, true)),
            )
            .await;
        assert!(!result.success);
        assert_eq!(result.output["execution_state"], "not_started");
        assert_eq!(result.output["command_started"], false);
        assert_eq!(result.output["failure_kind"], "invalid_arguments");
    }
    assert!(
        probe_patch_agent_request(&runtime, "process-sync-wait-bounds")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn run_process_short_timeout_stays_direct_without_job_headroom() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project = register_process_job_agent(&runtime, "process-short-timeout", temp.path()).await;
    let auth = auth_context(None, true);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::RunProcess {
                        project,
                        executable: "argv-helper".to_string(),
                        args: Vec::new(),
                        stdin: None,
                        session_id: None,
                        timeout_secs: Some(1),
                        sync_wait_secs: None,
                        cwd: None,
                        purpose: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "process-short-timeout").await;
    assert_eq!(request.kind, "run_process");
    assert!(request.job_id.is_none());
    complete_process_lifecycle(
        &runtime,
        "process-short-timeout",
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
    assert!(result.output.get("execution_state").is_none());
    assert!(result.output.get("promoted_to_job").is_none());
    assert!(runtime.runner_registry.list_jobs(Some(10)).await.is_empty());
}

#[tokio::test]
async fn stop_job_stops_the_promoted_process_without_starting_a_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime().with_structured_execution_sync_wait(Duration::from_millis(30));
    let project = register_process_job_agent(&runtime, "process-stop-job", temp.path()).await;
    let auth = auth_context(None, true);
    let (task, start_request) = dispatch_process_until_request(
        &runtime,
        "process-stop-job",
        process_call(project.clone(), None),
        auth.clone(),
    )
    .await;
    update_process_job(
        &runtime,
        "process-stop-job",
        &start_request,
        "running",
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    let handoff = task.await.unwrap();
    let job_id = handoff.output["job_id"].as_str().unwrap().to_string();
    assert_eq!(handoff.output["promoted_to_job"], true);

    let stopped = runtime
        .dispatch_with_auth(
            ToolCall::StopJob {
                project,
                job_id: job_id.clone(),
                session_id: None,
                confirm: true,
            },
            Some(&auth),
        )
        .await;
    assert!(stopped.success, "{:?}", stopped.error);
    let stop_request = wait_for_patch_agent_request(&runtime, "process-stop-job").await;
    assert_eq!(stop_request.kind, "stop_job");
    assert_eq!(stop_request.job_id.as_deref(), Some(job_id.as_str()));
    update_process_job(
        &runtime,
        "process-stop-job",
        &start_request,
        "stopped",
        Some(ShellCommandExecutionState::Completed),
        Some(-1),
        None,
        Some("job stopped by request\n"),
        Some("job stopped"),
    )
    .await;
    let terminal = runtime.runner_registry.get_job(&job_id).await.unwrap();
    assert_eq!(terminal.status, "stopped");
    assert_eq!(
        terminal.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    assert!(
        probe_patch_agent_request(&runtime, "process-stop-job")
            .await
            .is_none(),
        "stopping the Job must never enqueue a replacement execution"
    );
}

#[tokio::test]
async fn promoted_process_inherits_the_initiating_session_without_a_second_tool_execution() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime().with_structured_execution_sync_wait(Duration::from_millis(30));
    let project = register_process_job_agent(&runtime, "process-session-job", temp.path()).await;
    let session = runtime.sessions.start_session_with_guards(
        Some(project.clone()),
        Some("structured process continuation".to_string()),
        SessionMode::Normal,
        sessions::SessionGuards::default(),
    );
    let auth = auth_context(None, true);
    let (task, request) = dispatch_process_until_request(
        &runtime,
        "process-session-job",
        process_call(project.clone(), Some(session.session_id.clone())),
        auth.clone(),
    )
    .await;
    update_process_job(
        &runtime,
        "process-session-job",
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
    let job_id = handoff.output["job_id"].as_str().unwrap();
    let job = runtime.runner_registry.get_job(job_id).await.unwrap();
    assert_eq!(job.session_id.as_deref(), Some(session.session_id.as_str()));
    assert_eq!(job.project_id.as_deref(), Some(project.as_str()));
    assert_eq!(job.purpose.as_deref(), Some("diagnostic"));
    assert_eq!(job.kind, "run_process");

    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(100))
        .unwrap();
    assert_eq!(
        summary
            .events
            .iter()
            .filter(|event| event.tool_name == "run_process" && event.kind == "tool_call_started")
            .count(),
        1,
        "handoff must not record a fake second model tool execution"
    );
    update_process_job(
        &runtime,
        "process-session-job",
        &request,
        "completed",
        Some(ShellCommandExecutionState::Completed),
        Some(0),
        None,
        None,
        None,
    )
    .await;
}

#[tokio::test]
async fn run_process_preserves_large_typed_argv_without_shell_parsing() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project = register_process_agent(&runtime, "process-large", temp.path(), true).await;
    let large_args = vec!["a".repeat(4_500), "b".repeat(4_500)];
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let large_args = large_args.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::RunProcess {
                        project,
                        executable: "argv-helper".to_string(),
                        args: large_args,
                        stdin: None,
                        session_id: None,
                        timeout_secs: Some(30),
                        sync_wait_secs: Some(30),
                        cwd: None,
                        purpose: None,
                    },
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });

    let request = wait_for_patch_agent_request(&runtime, "process-large").await;
    assert_eq!(request.command, "");
    let process = request.process.as_ref().unwrap();
    assert_eq!(process.args, large_args);
    complete_process_lifecycle(
        &runtime,
        "process-large",
        request.request_id,
        ShellCommandExecutionState::Completed,
        Some(0),
        "",
        "",
        None,
    )
    .await;
    assert!(task.await.unwrap().success);
}

#[tokio::test]
async fn run_process_unsafe_batch_argument_has_stable_prestart_contract() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project =
        register_process_agent(&runtime, "process-batch-rejected", temp.path(), true).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    process_sync_call(project, None),
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "process-batch-rejected").await;
    complete_process_lifecycle(
        &runtime,
        "process-batch-rejected",
        request.request_id,
        ShellCommandExecutionState::NotStarted,
        None,
        "",
        "",
        Some(
            "invalid_arguments: Windows batch arguments cannot contain quotes; use a native runtime executable for these arguments",
        ),
    )
    .await;

    let result = task.await.unwrap();
    assert!(!result.success);
    assert_eq!(result.output["execution_state"], "not_started");
    assert_eq!(result.output["command_started"], false);
    assert_eq!(result.output["command_completed"], false);
    assert_eq!(result.output["failure_kind"], "invalid_arguments");
    assert!(result
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("native runtime"));
}

#[tokio::test]
async fn authority_denied_run_process_has_prestart_lifecycle() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime().with_permission_evaluator(
        permissions::PermissionEvaluator::with_mode(permissions::AuthorityMode::Restricted),
    );
    let project = register_process_agent(&runtime, "process-authority", temp.path(), true).await;

    let result = runtime
        .dispatch_with_auth(process_call(project, None), Some(&auth_context(None, true)))
        .await;

    assert!(!result.success);
    assert_eq!(result.output["execution_state"], "not_started");
    assert_eq!(result.output["command_started"], false);
    assert_eq!(result.output["command_completed"], false);
    assert_eq!(result.output["failure_kind"], "permission_denied");
    assert!(probe_patch_agent_request(&runtime, "process-authority")
        .await
        .is_none());
}

#[tokio::test]
async fn run_process_transport_uncertainty_and_timeout_preserve_phase_a_truth() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project = register_process_agent(&runtime, "process-lifecycle", temp.path(), true).await;
    let uncertain_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    process_sync_call(project, None),
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    wait_for_patch_agent_request(&runtime, "process-lifecycle").await;
    runtime
        .runner_registry
        .reconcile_disconnect("process-lifecycle", "inst")
        .await;
    let uncertain = uncertain_task.await.unwrap();
    assert!(!uncertain.success);
    assert_eq!(uncertain.output["execution_state"], "outcome_unknown");
    assert_eq!(uncertain.output["command_started"], true);
    assert_eq!(uncertain.output["command_completed"], false);
    assert!(uncertain
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("Do not automatically retry"));

    let timeout_runtime = test_runtime();
    let timeout_project =
        register_process_agent(&timeout_runtime, "process-timeout", temp.path(), true).await;
    let timeout_task = tokio::spawn({
        let runtime = timeout_runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    process_sync_call(timeout_project, None),
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&timeout_runtime, "process-timeout").await;
    complete_process_lifecycle(
        &timeout_runtime,
        "process-timeout",
        request.request_id,
        ShellCommandExecutionState::TimedOut,
        Some(-1),
        "",
        "process timed out",
        Some("process timed out"),
    )
    .await;
    let timed_out = timeout_task.await.unwrap();
    assert!(!timed_out.success);
    assert_eq!(timed_out.output["execution_state"], "timed_out");
    assert_eq!(timed_out.output["command_started"], true);
    assert_eq!(timed_out.output["command_completed"], false);
    assert!(timed_out
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("do not blindly retry"));
}

#[tokio::test]
async fn run_process_session_default_cwd_applies_without_default_shell() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let frontend = root.join("frontend");
    std::fs::create_dir_all(&frontend).unwrap();
    let runtime = test_runtime();
    let project = register_process_agent(&runtime, "process-context", &root, true).await;
    let session = runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(project.clone()),
                Some("process context".to_string()),
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
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    process_sync_call(project, Some(session_id)),
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });

    let request = wait_for_patch_agent_request(&runtime, "process-context").await;
    assert_eq!(
        request.cwd.as_deref(),
        Some(frontend.to_string_lossy().as_ref())
    );
    assert_eq!(request.command, "");
    assert_eq!(request.process.as_ref().unwrap().executable, "argv-helper");
    complete_process_lifecycle(
        &runtime,
        "process-context",
        request.request_id,
        ShellCommandExecutionState::Completed,
        Some(0),
        "",
        "",
        None,
    )
    .await;
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert!(result.output.get("cwd").is_none());
    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    let event = finished_event(&summary, "run_process");
    assert_eq!(event.risk_class, "job_run");
    assert!(event.shell_like);
    let started = summary
        .events
        .iter()
        .find(|event| event.kind == "tool_call_started" && event.tool_name == "run_process")
        .expect("run_process start event");
    let input = started
        .input_summary
        .as_ref()
        .expect("process input summary");
    assert_eq!(input["executable_present"], true);
    assert_eq!(input["arg_count"], 3);
    assert_eq!(input["stdin_present"], true);
    assert!(input.get("executable").is_none());
    assert!(input.get("args").is_none());
    assert!(input.get("stdin").is_none());
    assert!(input.get("process_summary").is_none());
}

#[tokio::test]
async fn run_process_named_ssh_resource_fails_before_enqueue() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project = register_process_agent(&runtime, "process-ssh", temp.path(), true).await;
    let session = runtime
        .sessions
        .start_session_with_options(
            sessions::SessionCreateOptions::new(
                Some(project.clone()),
                Some("remote process".to_string()),
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

    let result = runtime
        .dispatch_with_auth(
            process_call(project, Some(session.session_id)),
            Some(&auth_context(None, true)),
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.output["execution_state"], "not_started");
    assert_eq!(result.output["command_started"], false);
    assert_eq!(result.output["command_completed"], false);
    assert_eq!(result.output["failure_kind"], "unsupported_resource");
    assert_eq!(result.output["error_kind"], "unsupported_resource");
    assert_eq!(result.output["recovery_kind"], "fix_input");
    assert!(result.output.get("recovery_tool").is_none());
    assert!(probe_patch_agent_request(&runtime, "process-ssh")
        .await
        .is_none());
}

#[tokio::test]
async fn run_process_validation_and_inspect_permission_boundaries_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let runtime = test_runtime();
    let project = register_process_agent(&runtime, "process-guards", &root, true).await;

    for invalid in [
        ToolCall::RunProcess {
            project: project.clone(),
            executable: String::new(),
            args: Vec::new(),
            stdin: None,
            session_id: None,
            timeout_secs: Some(30),
            sync_wait_secs: None,
            cwd: None,
            purpose: None,
        },
        ToolCall::RunProcess {
            project: project.clone(),
            executable: "sh".to_string(),
            args: vec!["-c".to_string(), "touch marker".to_string()],
            stdin: None,
            session_id: None,
            timeout_secs: Some(30),
            sync_wait_secs: None,
            cwd: None,
            purpose: None,
        },
        ToolCall::RunProcess {
            project: project.clone(),
            executable: "argv-helper".to_string(),
            args: vec!["bad\0arg".to_string()],
            stdin: None,
            session_id: None,
            timeout_secs: Some(30),
            sync_wait_secs: None,
            cwd: None,
            purpose: None,
        },
        ToolCall::RunProcess {
            project: project.clone(),
            executable: "argv-helper".to_string(),
            args: vec![String::new(); 257],
            stdin: None,
            session_id: None,
            timeout_secs: Some(30),
            sync_wait_secs: None,
            cwd: None,
            purpose: None,
        },
        ToolCall::RunProcess {
            project: project.clone(),
            executable: "argv-helper".to_string(),
            args: vec!["x".repeat(8_193)],
            stdin: None,
            session_id: None,
            timeout_secs: Some(30),
            sync_wait_secs: None,
            cwd: None,
            purpose: None,
        },
        ToolCall::RunProcess {
            project: project.clone(),
            executable: "argv-helper".to_string(),
            args: Vec::new(),
            stdin: None,
            session_id: None,
            timeout_secs: Some(0),
            sync_wait_secs: None,
            cwd: None,
            purpose: None,
        },
        ToolCall::RunProcess {
            project: project.clone(),
            executable: "argv-helper".to_string(),
            args: Vec::new(),
            stdin: None,
            session_id: None,
            timeout_secs: Some(30),
            sync_wait_secs: None,
            cwd: Some("bad\0cwd".to_string()),
            purpose: None,
        },
        ToolCall::RunProcess {
            project: project.clone(),
            executable: "argv-helper".to_string(),
            args: Vec::new(),
            stdin: Some("x".repeat(65_537)),
            session_id: None,
            timeout_secs: Some(30),
            sync_wait_secs: None,
            cwd: None,
            purpose: None,
        },
    ] {
        let result = runtime
            .dispatch_with_auth(invalid, Some(&auth_context(None, true)))
            .await;
        assert!(!result.success);
        assert_eq!(result.output["execution_state"], "not_started");
        assert_eq!(result.output["command_started"], false);
        assert_eq!(result.output["command_completed"], false);
        assert_eq!(result.output["failure_kind"], "invalid_arguments");
    }
    let read_only = runtime.sessions.start_session_with_guards(
        Some(project.clone()),
        Some("read-only process".to_string()),
        SessionMode::ReadOnly,
        sessions::SessionGuards::default(),
    );
    let denied = runtime
        .dispatch_with_auth(
            process_call(project, Some(read_only.session_id)),
            Some(&auth_context(None, true)),
        )
        .await;
    assert!(!denied.success);
    assert_eq!(denied.output["guard"], "deny_shell_tools");
    assert_eq!(denied.output["execution_state"], "not_started");
    assert_eq!(denied.output["command_started"], false);
    assert_eq!(denied.output["command_completed"], false);
    assert_eq!(denied.output["failure_kind"], "session_guard_denied");
    assert!(probe_patch_agent_request(&runtime, "process-guards")
        .await
        .is_none());
}

#[tokio::test]
async fn closed_session_run_process_has_prestart_lifecycle() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project = register_process_agent(&runtime, "process-closed", temp.path(), true).await;
    let session = runtime.sessions.start_session_with_guards(
        Some(project.clone()),
        Some("closed process".to_string()),
        SessionMode::Normal,
        sessions::SessionGuards::default(),
    );
    runtime.sessions.close_session(&session.session_id).unwrap();

    let result = runtime
        .dispatch_with_auth(
            process_call(project, Some(session.session_id)),
            Some(&auth_context(None, true)),
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "session_closed");
    assert_eq!(result.output["execution_state"], "not_started");
    assert_eq!(result.output["command_started"], false);
    assert_eq!(result.output["command_completed"], false);
    assert_eq!(result.output["failure_kind"], "session_closed");
    assert!(probe_patch_agent_request(&runtime, "process-closed")
        .await
        .is_none());
}

#[tokio::test]
async fn model_facing_session_denials_keep_run_process_prestart_lifecycle() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project =
        register_process_agent(&runtime, "process-kernel-guards", temp.path(), true).await;
    let read_only = runtime.sessions.start_session_with_guards(
        Some(project.clone()),
        Some("read-only model process".to_string()),
        SessionMode::ReadOnly,
        sessions::SessionGuards::default(),
    );
    let closed = runtime.sessions.start_session_with_guards(
        Some(project.clone()),
        Some("closed model process".to_string()),
        SessionMode::Normal,
        sessions::SessionGuards::default(),
    );
    runtime.sessions.close_session(&closed.session_id).unwrap();
    let auth = auth_context(None, true);

    for (session_id, failure_kind) in [
        (read_only.session_id.as_str(), "session_guard_denied"),
        (closed.session_id.as_str(), "session_closed"),
    ] {
        let outcome = runtime
            .call_tool_with_context(
                ToolCallRequest {
                    tool_name: "run_process".to_string(),
                    arguments: json!({
                        "project": project.clone(),
                        "executable": "argv-helper",
                        "args": [],
                        "session_id": session_id
                    }),
                },
                ToolCallContext {
                    transport: ToolTransport::Api,
                    session_id: None,
                    auth: Some(&auth),
                    window: None,
                    record_oauth_scope_denials: true,
                    host_file_import_trust:
                        crate::tool_runtime::kernel::HostFileImportTrust::Untrusted,
                },
            )
            .await;
        let result = outcome.result.expect("structured Session denial");
        assert!(!result.success);
        assert_eq!(result.output["execution_state"], "not_started");
        assert_eq!(result.output["command_started"], false);
        assert_eq!(result.output["command_completed"], false);
        assert_eq!(result.output["failure_kind"], failure_kind);
    }
    assert!(
        probe_patch_agent_request(&runtime, "process-kernel-guards")
            .await
            .is_none(),
        "model-facing Session denials must happen before Runner enqueue"
    );
}
