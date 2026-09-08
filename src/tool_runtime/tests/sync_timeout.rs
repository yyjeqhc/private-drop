//! Synchronous timeout contract for cargo_* and run_shell.
//!
//! Read-only structured validation tools (`cargo_check`, `cargo_test`,
//! `cargo_fmt(check=true)`) now define `timeout_secs` as the total runtime
//! budget of the command (1..=3600). Short validations return immediately; a
//! long validation continues as a Job and returns `job_id`. `run_shell` keeps
//! the public 1..=120 total-timeout contract; explicit values above 60 may use
//! the same durable Job execution while the default 60-second call stays sync.

use super::support::*;
use crate::runner_protocol::{
    RunnerCapabilities, RunnerPollRequest, RunnerResultRequest, ShellCommandExecutionState,
};
use crate::tool_runtime::helpers::{
    resolve_sync_timeout_secs, DEFAULT_RUN_SHELL_TIMEOUT_SECS, MIN_SYNC_TIMEOUT_SECS,
    SYNC_VALIDATION_WAIT_SECS,
};
use crate::tool_runtime::validation_events::validation_summary_for_session;
use crate::tool_runtime::{SessionMode, ToolCall, ToolResult};
use serde_json::json;

fn assert_timeout_rejected(result: &ToolResult, tool_name: &str) {
    assert!(
        !result.success,
        "{tool_name} should reject out-of-range timeout"
    );
    assert_eq!(result.output["command_started"], false);
    assert_eq!(result.output["failure_kind"], "invalid_arguments");
    assert_eq!(result.output["tool_failure"], true);
    assert!(result.output["exit_code"].is_null());
    let error = result.error.as_deref().unwrap_or("");
    assert!(
        error.contains(tool_name),
        "error should name calling tool {tool_name}: {error}"
    );
    assert!(
        error.contains("timeout_secs") && error.contains(&MIN_SYNC_TIMEOUT_SECS.to_string()),
        "error should describe the timeout range: {error}"
    );
    assert!(
        !error.to_ascii_lowercase().contains("runshell"),
        "error must not leak runShell implementation detail: {error}"
    );
    // "run_shell" is allowed only when it is the calling tool name.
    if tool_name != "run_shell" {
        assert!(
            !error.contains("run_shell"),
            "error must not leak run_shell implementation detail: {error}"
        );
    }
}

async fn assert_no_pending_shell_request(
    runtime: &crate::tool_runtime::ToolRuntime,
    client_id: &str,
) {
    let req = runtime
        .runner_registry
        .poll(RunnerPollRequest {
            client_id: client_id.to_string(),
            runner_instance_id: "inst".to_string(),
        })
        .await
        .expect("poll should succeed");
    assert!(
        req.is_none(),
        "out-of-range timeout must not enqueue agent shell request: {req:?}"
    );
}

#[test]
fn resolve_sync_timeout_secs_rejects_out_of_range() {
    assert_eq!(
        resolve_sync_timeout_secs(None, DEFAULT_RUN_SHELL_TIMEOUT_SECS).unwrap(),
        DEFAULT_RUN_SHELL_TIMEOUT_SECS
    );
    assert_eq!(resolve_sync_timeout_secs(Some(1), 120).unwrap(), 1);
    assert_eq!(resolve_sync_timeout_secs(Some(120), 120).unwrap(), 120);
    assert!(resolve_sync_timeout_secs(Some(0), 120).is_err());
    assert!(resolve_sync_timeout_secs(Some(121), 120).is_err());
    assert!(resolve_sync_timeout_secs(Some(300), 120).is_err());
    assert!(resolve_sync_timeout_secs(Some(600), 60).is_err());
}

#[test]
fn structured_validation_sync_grace_is_sixty_seconds() {
    assert_eq!(SYNC_VALIDATION_WAIT_SECS, 60);
}

fn assert_sync_wait_rejected(result: &ToolResult, tool_name: &str) {
    assert!(!result.success, "{tool_name} sync wait should reject");
    assert_eq!(result.output["execution_state"], "not_started");
    assert_eq!(result.output["command_started"], false);
    assert_eq!(result.output["failure_kind"], "invalid_arguments");
    assert_eq!(result.output["tool_failure"], true);
    let error = result.error.as_deref().unwrap_or_default();
    assert!(error.contains(tool_name), "{error}");
    assert!(error.contains("sync_wait_secs"), "{error}");
}

#[tokio::test]
async fn cargo_fmt_check_accepts_long_total_runtime_budget_and_hands_off() {
    // cargo_check and cargo_test long-budget promotion lifecycles are owned by
    // validation_handoff.rs. Keep only cargo_fmt(check=true)'s distinct branch.
    let client_id = "sync-timeout-cargo-fmt-long";
    let runtime = runtime_with_agent_project(client_id)
        .with_validation_sync_wait(std::time::Duration::from_millis(10));
    let caps = RunnerCapabilities {
        async_shell_jobs: true,
        structured_validation_argv: true,
        ..Default::default()
    };
    register_agent(&runtime, client_id, None, caps).await;
    let project = agent_test_project_id(client_id);
    let timeout = 300u64;

    let result = runtime
        .cargo_fmt_with_context(
            project,
            None,
            Some(true),
            Some(timeout),
            Some(1),
            None,
            None,
            None,
        )
        .await;
    assert!(
        result.success,
        "cargo_fmt(check=true) long budget should be accepted: {:?}",
        result.error
    );
    assert!(result.output["promoted_to_job"].as_bool().unwrap_or(false));
    assert_eq!(result.output["effective_timeout_secs"], timeout);
    assert_eq!(result.output["sync_wait_secs"], 1);
    let job_id = result.output["job_id"].as_str().unwrap().to_string();
    let status = runtime.job_status_for_auth(job_id, false, None).await;
    assert!(status.success, "{:?}", status.error);
}

#[tokio::test]
async fn cargo_validation_tools_reject_timeout_outside_1_3600() {
    let runtime = runtime_with_agent_project("sync-timeout-cargo-range");
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, "sync-timeout-cargo-range", None, caps).await;
    let project = agent_test_project_id("sync-timeout-cargo-range");

    for (tool_name, timeout) in [
        ("cargo_check", 0u64),
        ("cargo_test", 0),
        ("cargo_fmt", 0),
        ("cargo_check", 3601),
        ("cargo_test", 3601),
        ("cargo_fmt", 3601),
    ] {
        let result = match tool_name {
            "cargo_check" => {
                runtime
                    .cargo_check(
                        project.clone(),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some(timeout),
                    )
                    .await
            }
            "cargo_test" => {
                runtime
                    .cargo_test(
                        project.clone(),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some(timeout),
                    )
                    .await
            }
            "cargo_fmt" => {
                runtime
                    .cargo_fmt(project.clone(), None, Some(true), Some(timeout))
                    .await
            }
            _ => unreachable!(),
        };
        assert!(!result.success, "{tool_name} {timeout} should be rejected");
        assert_eq!(result.output["failure_kind"], "invalid_arguments");
        assert_no_pending_shell_request(&runtime, "sync-timeout-cargo-range").await;
    }
}

#[tokio::test]
async fn structured_validation_sync_wait_rejects_invalid_or_over_budget_values_before_enqueue() {
    let client_id = "sync-wait-validation-range";
    let runtime = runtime_with_agent_project(client_id);
    register_agent(&runtime, client_id, None, RunnerCapabilities::default()).await;
    let project = agent_test_project_id(client_id);
    let auth = auth_context(None, true);

    for (tool_name, sync_wait_secs, timeout_secs, extra) in [
        ("cargo_check", 0u64, 600u64, json!({})),
        ("cargo_check", 61, 600, json!({})),
        ("cargo_check", 31, 30, json!({})),
        ("cargo_test", 0, 600, json!({})),
        ("cargo_test", 61, 600, json!({})),
        ("cargo_test", 31, 30, json!({})),
        ("go_test", 0, 600, json!({})),
        ("go_test", 61, 600, json!({})),
        ("go_test", 31, 30, json!({})),
        ("cargo_fmt", 0, 600, json!({"check": true})),
        ("cargo_fmt", 61, 600, json!({"check": true})),
        ("cargo_fmt", 31, 30, json!({"check": true})),
    ] {
        let mut args = json!({
            "project": project,
            "timeout_secs": timeout_secs,
            "sync_wait_secs": sync_wait_secs,
        });
        if let Some(extra) = extra.as_object() {
            args.as_object_mut().unwrap().extend(extra.clone());
        }
        let call = ToolCall::from_tool_name(tool_name, args).unwrap();
        let result = runtime.dispatch_with_auth(call, Some(&auth)).await;
        assert_sync_wait_rejected(&result, tool_name);
        assert_no_pending_shell_request(&runtime, client_id).await;
    }
}

#[tokio::test]
async fn cargo_fmt_mutating_rejects_sync_wait_before_enqueue_or_job_creation() {
    let client_id = "sync-wait-fmt-mutating";
    let runtime = runtime_with_agent_project(client_id);
    register_agent(&runtime, client_id, None, RunnerCapabilities::default()).await;
    let project = agent_test_project_id(client_id);
    let auth = auth_context(None, true);

    for args in [
        json!({"project": project, "check": false, "timeout_secs": 120, "sync_wait_secs": 1}),
        json!({"project": project, "timeout_secs": 120, "sync_wait_secs": 1}),
    ] {
        let call = ToolCall::from_tool_name("cargo_fmt", args).unwrap();
        let result = runtime.dispatch_with_auth(call, Some(&auth)).await;
        assert_sync_wait_rejected(&result, "cargo_fmt");
        assert_no_pending_shell_request(&runtime, client_id).await;
        assert!(runtime.runner_registry.list_jobs(Some(10)).await.is_empty());
    }
}

#[tokio::test]
async fn cargo_fmt_mutating_rejects_timeout_above_120_before_enqueue() {
    let client_id = "sync-timeout-fmt-mutating";
    let runtime = runtime_with_agent_project(client_id);
    register_agent(&runtime, client_id, None, RunnerCapabilities::default()).await;
    let project = agent_test_project_id(client_id);

    // The successful 120-second synchronous lifecycle is owned by
    // validation_handoff::cargo_fmt_mutating_never_auto_promotes.
    for check in [Some(false), None] {
        let rejected = runtime
            .cargo_fmt(project.clone(), None, check, Some(121))
            .await;
        assert_timeout_rejected(&rejected, "cargo_fmt");
        assert_no_pending_shell_request(&runtime, client_id).await;
    }
}

#[tokio::test]
async fn run_shell_rejects_timeout_above_120_before_enqueue() {
    let runtime = runtime_with_agent_project("sync-timeout-shell");
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, "sync-timeout-shell", None, caps).await;
    let project = agent_test_project_id("sync-timeout-shell");

    for timeout in [121u64, 300] {
        let result = runtime
            .run_shell(project.clone(), "echo hi".to_string(), Some(timeout), None)
            .await;
        assert_timeout_rejected(&result, "run_shell");
        assert_no_pending_shell_request(&runtime, "sync-timeout-shell").await;
    }
}

#[tokio::test]
async fn dispatched_shared_capture_wait_timeout_reports_outcome_unknown_without_job() {
    // The server's result-wait timeout is not proof that the Runner command
    // reached its own timeout. Once dispatched, the final outcome is unknown
    // and the synchronous caller must not be invited to retry blindly.
    let client_id = "sync-short-full-test";
    let runtime = runtime_with_agent_project(client_id);
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, client_id, None, caps).await;
    let project = agent_test_project_id(client_id);
    let auth = auth_context(None, true);
    let session = runtime.sessions.start_session(Some(project.clone()), None);
    let session_id = session.session_id.clone();

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::CargoTest {
                        project,
                        session_id: Some(session_id),
                        cwd: None,
                        filter: None,
                        all_targets: None,
                        all_features: None,
                        no_default_features: None,
                        features: None,
                        package: None,
                        no_run: None,
                        require_tests: None,
                        min_tests: None,
                        timeout_secs: Some(1),
                        sync_wait_secs: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, client_id).await;
    assert_eq!(request.command, "cargo test");
    let active_summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .expect("active session summary");
    assert!(active_summary
        .events
        .iter()
        .any(|event| { event.kind == "tool_call_started" && event.tool_name == "cargo_test" }));

    let result = task.await.unwrap();
    assert!(!result.success);
    assert_eq!(result.output["execution_state"], "outcome_unknown");
    assert_eq!(result.output["failure_kind"], "outcome_unknown");
    assert_eq!(result.output["command_started"], true);
    assert_eq!(result.output["command_completed"], false);
    assert_eq!(result.output["terminal"], false);
    assert_eq!(result.output["passed"], false);
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("Do not automatically retry")));

    let client = runtime
        .runner_registry
        .get_runner_view(client_id)
        .await
        .expect("registered client");
    assert_eq!(client.pending_requests, 0);
    assert!(runtime.runner_registry.list_jobs(Some(10)).await.is_empty());
    let late = runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: client_id.to_string(),
            runner_instance_id: "inst".to_string(),
            request_id: request.request_id,
            exit_code: Some(0),
            stdout: Some("late result".to_string()),
            stderr: Some(String::new()),
            duration_ms: Some(3_000),
            error: None,
        })
        .await
        .unwrap_err();
    assert!(late.contains("unknown or expired shell request"), "{late}");

    let finished_summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .expect("finished session summary");
    assert!(finished_summary.events.iter().any(|event| {
        event.kind == "tool_call_finished"
            && event.tool_name == "cargo_test"
            && event.status.as_deref() == Some("failed")
    }));
}

#[tokio::test]
async fn undispatched_shared_capture_wait_timeout_reports_not_started() {
    let client_id = "sync-timeout-undispatched";
    let runtime = runtime_with_agent_project(client_id);
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, client_id, None, caps).await;
    let project = agent_test_project_id(client_id);

    let output = runtime
        .run_project_command_capture(&project, "cargo check".to_string(), 1, None)
        .await
        .expect("capture wait timeout is a lifecycle result");

    assert_eq!(
        output.execution_state,
        ShellCommandExecutionState::NotStarted
    );
    assert!(output.exit_code.is_none());
    assert!(output
        .error
        .as_deref()
        .is_some_and(|error| error.contains("timed out waiting 1 seconds for agent shell result")));
    let client = runtime
        .runner_registry
        .get_runner_view(client_id)
        .await
        .expect("registered client");
    assert_eq!(client.pending_requests, 0);
}

#[tokio::test]
async fn shared_capture_missing_pending_record_reports_outcome_unknown() {
    let client_id = "sync-timeout-missing-record";
    let runtime = runtime_with_agent_project(client_id);
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, client_id, None, caps).await;
    let project = agent_test_project_id(client_id);

    let capture = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .run_project_command_capture(&project, "cargo check".to_string(), 60, None)
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, client_id).await;
    assert_eq!(
        runtime
            .runner_registry
            .cancel_request_dispatch_state(&request.request_id)
            .await,
        Some(true)
    );

    let output = capture
        .await
        .unwrap()
        .expect("a dropped waiter after dispatch is a lifecycle result");
    assert_eq!(
        output.execution_state,
        ShellCommandExecutionState::OutcomeUnknown
    );
    assert!(output
        .error
        .as_deref()
        .is_some_and(|error| error.contains("after dispatch may have occurred")));
}

#[tokio::test]
async fn timeout_rejection_does_not_pollute_validation_summary() {
    let runtime = runtime_with_agent_project("sync-timeout-ledger");
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, "sync-timeout-ledger", None, caps).await;
    let project = agent_test_project_id("sync-timeout-ledger");
    let auth = auth_context(None, true);

    let session = runtime
        .dispatch_with_auth(
            ToolCall::StartSession {
                project: Some(project.clone()),
                title: Some("timeout contract".to_string()),
                mode: SessionMode::Normal,
                deny_write_tools: false,
                deny_shell_tools: false,
                execution_context: None,
            },
            Some(&auth),
        )
        .await;
    assert!(session.success, "{:?}", session.error);
    let session_id = session.output["session_id"]
        .as_str()
        .or_else(|| session.output["session"]["session_id"].as_str())
        .expect("session id")
        .to_string();

    let rejected = runtime
        .dispatch_with_auth(
            ToolCall::CargoCheck {
                project: project.clone(),
                session_id: Some(session_id.clone()),
                cwd: None,
                all_targets: Some(true),
                all_features: None,
                no_default_features: None,
                features: None,
                package: None,
                timeout_secs: Some(0),
                sync_wait_secs: None,
            },
            Some(&auth),
        )
        .await;
    assert!(!rejected.success);
    assert_eq!(rejected.output["failure_kind"], "invalid_arguments");

    let summary_after_reject = runtime
        .sessions
        .summary(&session_id, Some(50))
        .expect("session summary");
    let validation_after_reject = validation_summary_for_session(&summary_after_reject);
    assert_eq!(validation_after_reject["available"], false);
    assert_eq!(validation_after_reject["status"], "not_run");
    assert_eq!(validation_after_reject["events_total"], 0);
    assert_eq!(validation_after_reject["historical_failures"]["count"], 0);

    // A subsequent valid cargo_check must pass and not be mixed by the reject.
    let check_task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::CargoCheck {
                        project,
                        session_id: Some(session_id),
                        cwd: None,
                        all_targets: Some(true),
                        all_features: None,
                        no_default_features: None,
                        features: None,
                        package: None,
                        timeout_secs: Some(60),
                        sync_wait_secs: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "sync-timeout-ledger").await;
    assert!(req.command.contains("cargo check"));
    complete_patch_agent_request(
        &runtime,
        "sync-timeout-ledger",
        &req.request_id,
        0,
        "Finished `dev` profile [unoptimized + debuginfo] target(s)\n",
        "",
    )
    .await;
    let check = check_task.await.unwrap();
    assert!(check.success, "{:?}", check.error);

    let summary = runtime
        .sessions
        .summary(&session_id, Some(50))
        .expect("session summary after success");
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["available"], true);
    assert_eq!(validation["status"], "passed");
    assert_eq!(validation["latest_status"], "passed");
    assert_eq!(validation["events_total"], 1);
    assert_eq!(validation["historical_failures"]["count"], 0);
    assert_eq!(validation["latest"]["tool_name"], "cargo_check");
    assert_eq!(validation["latest"]["success"], true);

    // Parameter rejection remains a normal tool failure event, not validation evidence.
    let finished = summary
        .events
        .iter()
        .filter(|e| e.kind == "tool_call_finished" && e.tool_name == "cargo_check")
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 2);
    let reject_event = finished
        .iter()
        .find(|e| e.status.as_deref() == Some("failed"))
        .expect("reject finished event");
    assert_eq!(
        reject_event.failure_kind.as_deref(),
        Some("invalid_arguments")
    );
    assert!(reject_event.validation_output_summary.is_none());
    assert!(reject_event.exit_code.is_none());
}
