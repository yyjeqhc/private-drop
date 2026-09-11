use super::support::*;
use crate::tool_runtime::sessions::SessionTransport;
use crate::tool_runtime::validation_parser::{NO_STABLE_DIAGNOSTICS_REASON, PARSER_KIND};
use crate::tool_runtime::{ExecutionPurpose, ExecutionShell, ToolCall};
use serde_json::{json, Value};

#[tokio::test]
async fn run_shell_declared_validation_enters_unified_summary_with_shell_and_root_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "validation-shell", "demo", tmp.path()).await;
    let auth = auth_context(None, true);
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("shell validation".to_string()));

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
                        command: "cargo test focused".to_string(),
                        session_id: Some(session_id),
                        timeout_secs: Some(30),
                        cwd: Some(".".to_string()),
                        purpose: Some(ExecutionPurpose::Test),
                        shell: Some(ExecutionShell::Bash),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "validation-shell").await;
    assert_eq!(request.kind, "run_shell");
    assert!(request.command.starts_with("exec bash -c "));
    complete_patch_agent_request(
        &runtime,
        "validation-shell",
        &request.request_id,
        0,
        "running 1 test\n\ntest result: ok. 1 passed; 0 failed; 0 ignored\n",
        "",
    )
    .await;
    let execution = task.await.unwrap();
    assert!(execution.success, "{:?}", execution.error);
    assert_eq!(execution.output["cwd"], ".");
    assert_eq!(execution.output["shell"], "bash");
    assert_eq!(execution.output["purpose"], "test");

    let summary = runtime
        .dispatch_with_auth(
            ToolCall::ValidationSummary {
                project,
                session_id: session.session_id,
                limit: Some(20),
            },
            Some(&auth),
        )
        .await;
    assert!(summary.success, "{:?}", summary.error);
    let event = &summary.output["validation"]["latest"];
    assert_eq!(event["execution_source"], "run_shell");
    assert_eq!(event["purpose"], "test");
    assert_eq!(event["validation_kind"], "test");
    assert_eq!(event["cwd"], ".");
    assert_eq!(event["shell"], "bash");
    assert_eq!(event["tests_run_count"], 1);
}
#[tokio::test]
async fn completed_run_job_validation_enters_handoff_from_job_authority() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let auth = open_auth_context();
    let capabilities = crate::runner_protocol::RunnerCapabilities {
        async_shell_jobs: true,
        ..Default::default()
    };
    register_agent_projects_for_auth(
        &runtime,
        "validation-job",
        &auth,
        capabilities,
        vec![registered_project("demo", &tmp.path().to_string_lossy())],
    )
    .await;
    let project = "agent:validation-job:demo".to_string();
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("job validation".to_string()));

    let assertion_name = "direct run job validation";
    let expected_identity =
        crate::tool_runtime::tool_audit::assertion_validation_identity(assertion_name);
    let (call, recorder_metadata) = ToolCall::from_tool_name_with_recorder_metadata(
        "run_job",
        json!({
            "project": project,
            "command": "cargo test focused",
            "session_id": session.session_id,
            "timeout_secs": 30,
            "cwd": ".",
            "purpose": "test",
            "shell": "bash",
            "assertion_name": assertion_name,
        }),
    )
    .unwrap();
    let execution = runtime
        .dispatch_with_auth_transport_options_and_metadata(
            call,
            Some(&auth),
            SessionTransport::Mcp,
            recorder_metadata,
        )
        .await;
    assert!(execution.success, "{:?}", execution.error);
    assert_eq!(execution.output["cwd"], ".");
    assert_eq!(execution.output["shell"], "bash");
    let job_id = execution.output["job_id"].as_str().unwrap().to_string();
    let request = wait_for_runner_request_for_client(&runtime, "validation-job").await;
    assert_eq!(request.kind, "start_job");
    runtime
        .runner_registry
        .update_job(crate::runner_protocol::RunnerJobUpdateRequest {
            client_id: "validation-job".to_string(),
            runner_instance_id: "inst-validation-job".to_string(),
            update_seq: None,
            job_id: job_id.clone(),
            request_id: Some(request.request_id),
            status: "completed".to_string(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: Some(
                "running 1 test\n\ntest result: ok. 1 passed; 0 failed; 0 ignored\n".to_string(),
            ),
            stderr_tail: Some(String::new()),
            log_snapshot: None,
            exit_code: Some(0),
            duration_ms: Some(12),
            error: None,
            command_execution_state: None,
            validation_progress: None,
            activity: None,
            finished: true,
        })
        .await
        .unwrap();

    let handoff = runtime
        .dispatch_with_auth(
            ToolCall::SessionHandoffSummary {
                session_id: session.session_id,
                project: Some(project),
                include_workspace: Some(false),
                include_checkpoints: Some(false),
                include_validation: Some(true),
                summary_only: true,
                limit: Some(20),
            },
            Some(&auth),
        )
        .await;
    assert!(handoff.success, "{:?}", handoff.error);
    assert_eq!(handoff.output["validation"]["status"], "passed");
    let event = &handoff.output["validation"]["latest"];
    assert_eq!(event["execution_source"], "run_job");
    assert_eq!(event["purpose"], "test");
    assert_eq!(event["execution_state"], "completed");
    assert_eq!(event["exit_code"], 0);
    assert_eq!(event["identity"], expected_identity);
    assert_eq!(event["assertion_name"], assertion_name);
    assert_eq!(
        handoff.output["facts"]["executions"][0]["identity"],
        event["identity"]
    );
    assert_eq!(handoff.output["hard_blockers"], json!([]));
}
#[tokio::test]
async fn promoted_run_process_cargo_test_materializes_canonical_validation_evidence() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime =
        test_runtime().with_structured_execution_sync_wait(std::time::Duration::from_millis(40));
    let auth = open_auth_context();
    register_agent_projects_for_auth(
        &runtime,
        "validation-process-job",
        &auth,
        crate::runner_protocol::RunnerCapabilities {
            shell: true,
            async_jobs: true,
            async_shell_jobs: true,
            structured_validation_argv: true,
            structured_process_argv: true,
            structured_execution_jobs: true,
            ..Default::default()
        },
        vec![registered_project("demo", &tmp.path().to_string_lossy())],
    )
    .await;
    let project = "agent:validation-process-job:demo".to_string();
    let session = runtime.sessions.start_session(Some(project.clone()), None);
    let session_id = session.session_id.clone();

    let assertion_name = "promoted process validation";
    let expected_identity =
        crate::tool_runtime::tool_audit::assertion_validation_identity(assertion_name);
    let (call, recorder_metadata) = ToolCall::from_tool_name_with_recorder_metadata(
        "run_process",
        json!({
            "project": project,
            "executable": "cargo",
            "args": ["test", "focused", "-p", "webcodex"],
            "session_id": session_id,
            "timeout_secs": 121,
            "cwd": ".",
            "purpose": "test",
            "assertion_name": assertion_name,
        }),
    )
    .unwrap();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth_transport_options_and_metadata(
                    call,
                    Some(&auth),
                    SessionTransport::Mcp,
                    recorder_metadata,
                )
                .await
        }
    });
    let request = wait_for_runner_request_for_client(&runtime, "validation-process-job").await;
    assert_eq!(request.kind, "start_process_job");
    assert_eq!(request.process.as_ref().unwrap().executable, "cargo");
    let handoff = task.await.unwrap();
    assert!(handoff.success, "{:?}", handoff.error);
    assert_eq!(handoff.output["promoted_to_job"], true);
    let job_id = handoff.output["job_id"].as_str().unwrap().to_string();
    let admitted = runtime.runner_registry.get_job(&job_id).await.unwrap();
    let metadata = admitted.structured_execution.as_ref().unwrap();
    assert_eq!(metadata.execution_source, "run_process");
    assert_eq!(metadata.validation_tool.as_deref(), Some("cargo_test"));
    assert_eq!(metadata.assertion_name.as_deref(), Some(assertion_name));
    let target = metadata
        .validation_identity
        .as_deref()
        .expect("admission-derived validation identity")
        .to_string();
    assert_eq!(target, expected_identity);
    assert!(target.starts_with("assertion:"));

    runtime
        .runner_registry
        .update_job(crate::runner_protocol::RunnerJobUpdateRequest {
            client_id: "validation-process-job".to_string(),
            runner_instance_id: "inst-validation-process-job".to_string(),
            update_seq: None,
            job_id: job_id.clone(),
            request_id: Some(request.request_id),
            status: "completed".to_string(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: Some(
                "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
                    .to_string(),
            ),
            stderr_tail: Some(String::new()),
            log_snapshot: None,
            exit_code: Some(0),
            duration_ms: Some(12),
            error: None,
            command_execution_state: Some(crate::runner_protocol::ShellCommandExecutionState::Completed),
            validation_progress: None,
            activity: None,
            finished: true,
        })
        .await
        .unwrap();

    let summary = runtime
        .dispatch_with_auth(
            ToolCall::ValidationSummary {
                project,
                session_id,
                limit: Some(20),
            },
            Some(&auth),
        )
        .await;
    assert!(summary.success, "{:?}", summary.error);
    let validation = &summary.output["validation"];
    assert_eq!(validation["status"], "passed");
    assert_eq!(validation["unresolved_failures"]["count"], 0);
    let latest = &validation["latest"];
    assert_eq!(latest["execution_source"], "run_process");
    assert_eq!(latest["validation_kind"], "test");
    assert_eq!(latest["identity"], target);
    assert_eq!(latest["assertion_name"], assertion_name);
    assert_eq!(latest["tests_run_count"], 1);
    assert_eq!(latest["tests_passed"], 1);
    assert_eq!(latest["tests_failed"], 0);
    assert_eq!(latest["zero_tests_run"], false);
}
#[tokio::test]
async fn finish_coding_task_validation_available_when_ledger_has_validation_events() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    commit_file(tmp.path(), "Cargo.toml", cargo_toml(), "add cargo manifest");
    commit_file(
        tmp.path(),
        "src/lib.rs",
        "pub fn value() -> i32 { 1 }\n",
        "add lib",
    );
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "validation-finish", "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let start = runtime
        .dispatch_with_auth(
            ToolCall::WorkOnProject {
                project: project.clone(),
                client_id: None,
                path: None,
                mode: None,
                base_ref: None,
                instruction: "validation finish".to_string(),
                session_id: None,
                include_project_instructions: true,
                include_workflow_guidance: true,
                include_extension_catalog: false,
            },
            Some(&auth),
        )
        .await;
    assert!(start.success, "{:?}", start.error);
    let session_id = start.output["session_id"].as_str().unwrap().to_string();

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
    let req = wait_for_patch_agent_request(&runtime, "validation-finish").await;
    assert!(req.command.contains("cargo check --all-targets"));
    complete_patch_agent_request(&runtime, "validation-finish", &req.request_id, 0, "", "").await;
    let check = check_task.await.unwrap();
    assert!(check.success, "{:?}", check.error);
    assert_eq!(check.output["permission"]["required"], true);
    assert_eq!(check.output["permission"]["status"], "auto_approved");
    assert_eq!(check.output["permission"]["risk"], "validation");

    let test_task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let project = project.clone();
        let session_id = session_id.clone();
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
                        timeout_secs: Some(60),
                        sync_wait_secs: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "validation-finish").await;
    assert!(req.command.contains("cargo test"));
    complete_patch_agent_request(
        &runtime,
        "validation-finish",
        &req.request_id,
        101,
        "running 1 test\n",
        "test failure details stay out of validation summary\n",
    )
    .await;
    let test = test_task.await.unwrap();
    assert!(!test.success);

    let finish_task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        async move {
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
                        include_validation_summary: Some(true),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "validation-finish").await;
    assert_internal_posix_script_contains(&req, "git status --porcelain=v1 -b");
    let show_changes_stdout =
        crate::tool_runtime::framed_clean_show_changes_test_stdout("add lib", false);
    complete_patch_agent_request(
        &runtime,
        "validation-finish",
        &req.request_id,
        0,
        &show_changes_stdout,
        "",
    )
    .await;
    let finish = finish_task.await.unwrap();
    assert!(finish.success, "{:?}", finish.error);

    let validation = &finish.output["validation"];
    assert_eq!(validation["available"], true);
    assert_eq!(validation["status"], "mixed");
    assert!(validation["reason"].is_null());
    assert_eq!(validation["source"], "session_ledger");
    assert_eq!(validation["events_total"], 2);
    assert_eq!(validation["successes"], 1);
    assert_eq!(validation["failures"], 1);
    assert_eq!(validation["latest_success"]["tool_name"], "cargo_check");
    assert_eq!(validation["latest_success"]["validation_kind"], "check");
    assert_eq!(validation["latest_success"]["exit_code"], 0);
    assert_eq!(
        validation["latest_success"]["summary"],
        "cargo_check succeeded"
    );
    assert_eq!(validation["latest_failure"]["tool_name"], "cargo_test");
    assert_eq!(validation["latest_failure"]["validation_kind"], "test");
    assert_eq!(validation["latest_failure"]["exit_code"], 101);
    assert_eq!(validation["latest_failure"]["summary"], "cargo_test failed");
    assert_eq!(validation["parser"]["available"], true);
    assert_eq!(validation["parser"]["kind"], PARSER_KIND);
    assert!(validation["parser"].get("reason").is_none());
    assert_eq!(
        validation["latest_failure"]["diagnostics"]["available"],
        false
    );
    assert_eq!(
        validation["latest_failure"]["diagnostics"]["reason"],
        NO_STABLE_DIAGNOSTICS_REASON
    );
    assert_no_raw_validation_output_fields(validation, "finish validation summary");

    let handoff = runtime
        .dispatch_with_auth(
            ToolCall::SessionHandoffSummary {
                session_id: session_id.clone(),
                project: None,
                include_workspace: Some(false),
                include_checkpoints: Some(false),
                include_validation: Some(true),
                summary_only: false,
                limit: None,
            },
            Some(&auth),
        )
        .await;
    assert!(handoff.success, "{:?}", handoff.error);
    assert_eq!(
        handoff.output["validation"], finish.output["validation"],
        "handoff validation should match finish_coding_task validation for the same session ledger"
    );
    assert_no_raw_validation_output_fields(
        &handoff.output["validation"],
        "handoff validation summary",
    );

    let handoff_compact = runtime
        .dispatch_with_auth(
            ToolCall::SessionHandoffSummary {
                session_id: session_id.clone(),
                project: None,
                include_workspace: Some(false),
                include_checkpoints: Some(false),
                include_validation: Some(true),
                summary_only: true,
                limit: None,
            },
            Some(&auth),
        )
        .await;
    assert!(handoff_compact.success, "{:?}", handoff_compact.error);
    assert_eq!(
        handoff_compact.output["validation"], finish.output["validation"],
        "summary_only handoff must preserve the full structured validation evidence"
    );

    let finish_compact_task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only: true,
                        include_diff: Some(false),
                        include_workspace: None,
                        include_hygiene: Some(false),
                        include_handoff: Some(false),
                        include_validation_summary: Some(true),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "validation-finish").await;
    let show_changes_stdout =
        crate::tool_runtime::framed_clean_show_changes_test_stdout("add lib", false);
    complete_patch_agent_request(
        &runtime,
        "validation-finish",
        &req.request_id,
        0,
        &show_changes_stdout,
        "",
    )
    .await;
    let finish_compact = finish_compact_task.await.unwrap();
    assert!(finish_compact.success, "{:?}", finish_compact.error);
    assert_eq!(finish_compact.output["validation"]["status"], "mixed");
    assert_eq!(finish_compact.output["validation"]["successes"], 1);
    assert_eq!(finish_compact.output["validation"]["failures"], 1);
    assert_eq!(
        finish_compact.output["validation"]["unresolved_failure_count"],
        1
    );
    assert!(finish_compact.output["validation"].get("events").is_none());
    assert!(finish_compact.output["validation"].get("latest").is_none());
}
fn json_contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key(key) || map.values().any(|value| json_contains_key(value, key))
        }
        Value::Array(values) => values.iter().any(|value| json_contains_key(value, key)),
        _ => false,
    }
}

fn assert_no_raw_validation_output_fields(value: &Value, context: &str) {
    for key in [
        "stdout",
        "stderr",
        "stdout_tail",
        "stderr_tail",
        "stdout_tail_excerpt",
        "stderr_tail_excerpt",
        "validation_output_summary",
    ] {
        assert!(
            !json_contains_key(value, key),
            "{context} must not include {key}: {value}"
        );
    }
}

fn cargo_toml() -> &'static str {
    "[package]\nname = \"validation-finish\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n"
}
