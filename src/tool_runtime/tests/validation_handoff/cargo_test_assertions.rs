//! Cargo test-count postcondition coverage across synchronous, Job, Session,
//! and incomplete-evidence paths.

use super::*;

#[tokio::test]
async fn fast_cargo_test_require_tests_rejects_ignored_only_and_records_failed_session_evidence() {
    let client_id = "vhandoff-fast-test-count";
    let runtime = runtime_with_agent_project(client_id)
        .with_validation_sync_wait(std::time::Duration::from_millis(300));
    register_agent(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            async_shell_jobs: true,
            structured_validation_argv: true,
            structured_cargo_test_count_assertion: true,
            structured_cargo_test_execution_policy: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id(client_id);
    let auth = auth_context(None, true);
    let session = runtime.sessions.start_session(Some(project.clone()), None);
    let session_id = session.session_id.clone();

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let session_id = session_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::CargoTest {
                        project,
                        session_id: Some(session_id),
                        cwd: None,
                        filter: Some("focused".to_string()),
                        all_targets: None,
                        all_features: None,
                        no_default_features: None,
                        features: None,
                        package: None,
                        no_run: None,
                        require_tests: Some(true),
                        min_tests: None,
                        timeout_secs: Some(600),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let (request, job_id) = poll_start_validation_job(&runtime, client_id).await;
    runtime
        .runner_registry
        .update_job(cargo_test_update(
            client_id,
            &request.request_id,
            &job_id,
            "completed",
            "running 1 test\n\ntest ignored_only ... ignored\n\ntest result: ok. 0 passed; 0 failed; 1 ignored\n",
            "",
            Some(0),
            completed_progress(),
            true,
        ))
        .await
        .unwrap();

    let result = task.await.unwrap();
    assert!(!result.success);
    assert_eq!(result.output["exit_code"], 0);
    assert_eq!(result.output["execution_state"], "completed");
    assert_eq!(result.output["passed"], false);
    assert_eq!(result.output["failure_kind"], "validation_failed");
    assert_eq!(result.output["promoted_to_job"], false);
    assert_eq!(
        result.output["test_count_assertion"]["reason_code"],
        "minimum_not_met"
    );
    assert_eq!(
        result.output["test_count_assertion"]["evidence_reason_code"],
        "complete_summary"
    );
    assert_eq!(result.output["tests_run_count"], 0);
    assert_eq!(result.output["zero_tests_run"], true);
    assert_eq!(result.output["test_count_assertion"]["actual_tests_run"], 0);
    assert_cargo_result_matches_schema("cargo_test", &result);
    assert!(
        runtime.list_jobs_for_auth(None, None, None).await.output["jobs"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(50))
        .unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["events_total"], 1);
    assert_eq!(validation["status"], "inconclusive");
    assert_eq!(validation["historical_failures"]["count"], 0);
    assert_eq!(validation["unresolved_failures"]["count"], 0);
    assert_eq!(validation["evidence_gaps"]["count"], 1);
    assert_eq!(validation["latest"]["success"], false);
    assert_eq!(validation["latest"]["execution_success"], true);
    assert_eq!(validation["latest"]["validation_passed"], true);
    assert_eq!(validation["latest"]["failure_class"], "evidence_assertion");
    assert_eq!(validation["latest"]["exit_code"], 0);
    assert_eq!(
        validation["latest"]["test_count_assertion"]["reason_code"],
        "minimum_not_met"
    );
}

#[tokio::test]
async fn handoff_cargo_test_count_gap_preserves_completed_job_and_inconclusive_session_validation()
{
    let client_id = "vhandoff-test-count";
    let runtime = runtime_with_agent_project(client_id)
        .with_validation_sync_wait(std::time::Duration::from_millis(50));
    register_agent(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            async_shell_jobs: true,
            structured_validation_argv: true,
            structured_cargo_test_count_assertion: true,
            structured_cargo_test_execution_policy: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id(client_id);
    let auth = auth_context(None, true);
    let session = runtime.sessions.start_session(Some(project.clone()), None);
    let session_id = session.session_id.clone();

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let session_id = session_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::CargoTest {
                        project,
                        session_id: Some(session_id),
                        cwd: None,
                        filter: Some("focused".to_string()),
                        all_targets: None,
                        all_features: None,
                        no_default_features: None,
                        features: None,
                        package: None,
                        no_run: None,
                        require_tests: Some(true),
                        min_tests: Some(2),
                        timeout_secs: Some(1800),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let (request, job_id) = poll_start_validation_job(&runtime, client_id).await;
    let validation = request
        .job_context
        .as_ref()
        .and_then(|context| context.validation.as_ref())
        .expect("durable validation metadata");
    assert_eq!(validation.minimum_tests, Some(2));
    assert_eq!(validation.require_tests, Some(true));
    assert_eq!(validation.no_run, None);
    let handoff = task.await.unwrap();
    assert!(handoff.success, "{:?}", handoff.error);
    assert_eq!(handoff.output["promoted_to_job"], true);
    assert_eq!(handoff.output["job_id"], job_id);

    runtime
        .runner_registry
        .update_job(cargo_test_update(
            client_id,
            &request.request_id,
            &job_id,
            "completed",
            "running 1 test\n\ntest ignored_only ... ignored\n\ntest result: ok. 0 passed; 0 failed; 1 ignored\n",
            "",
            Some(0),
            completed_progress(),
            true,
        ))
        .await
        .unwrap();

    let status = runtime
        .job_status_for_auth(job_id.clone(), false, Some(&auth))
        .await;
    assert!(status.success, "{:?}", status.error);
    assert_eq!(status.output["status"], "completed");
    assert_eq!(status.output["exit_code"], 0);
    assert_eq!(status.output["validation"]["passed"], false);
    assert_eq!(
        status.output["validation"]["test_count_assertion"]["reason_code"],
        "minimum_not_met"
    );
    assert_eq!(
        status.output["validation"]["test_count_assertion"]["evidence_reason_code"],
        "complete_summary"
    );
    assert_eq!(
        status.output["validation"]["test_count_assertion"]["minimum_tests"],
        2
    );
    assert_eq!(
        status.output["validation"]["test_count_assertion"]["actual_tests_run"],
        0
    );

    let log = runtime
        .job_log_for_auth(job_id.clone(), None, Some(200), Some(&auth), None, None)
        .await;
    assert!(log.success, "{:?}", log.error);
    assert_eq!(log.output["status"], "completed");
    assert_eq!(log.output["exit_code"], 0);
    assert_eq!(log.output["validation"]["passed"], false);
    assert_eq!(
        log.output["validation"]["test_count_assertion"]["reason_code"],
        "minimum_not_met"
    );

    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(50))
        .unwrap();
    let validation = runtime
        .validation_summary_for_session_with_jobs(&summary, 50, Some(&auth))
        .await;
    assert_eq!(validation["status"], "inconclusive");
    assert_eq!(validation["historical_failures"]["count"], 0);
    assert_eq!(validation["unresolved_failures"]["count"], 0);
    assert_eq!(validation["evidence_gaps"]["count"], 1);
    assert_eq!(validation["latest"]["success"], false);
    assert_eq!(validation["latest"]["execution_success"], true);
    assert_eq!(validation["latest"]["validation_passed"], true);
    assert_eq!(validation["latest"]["failure_class"], "evidence_assertion");
    assert_eq!(validation["latest"]["exit_code"], 0);
    assert_eq!(
        validation["latest"]["test_count_assertion"]["reason_code"],
        "minimum_not_met"
    );
}

#[tokio::test]
async fn cargo_test_minimum_misassertion_then_sufficient_same_target_is_non_blocking_at_handoff() {
    let client_id = "vhandoff-minimum-misassertion-closeout";
    let runtime = runtime_with_agent_project(client_id)
        .with_validation_sync_wait(std::time::Duration::from_millis(300));
    register_agent(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            async_shell_jobs: true,
            structured_validation_argv: true,
            structured_cargo_test_count_assertion: true,
            structured_cargo_test_execution_policy: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id(client_id);
    let auth = auth_context(None, true);
    let session = runtime.sessions.start_session(Some(project.clone()), None);
    let session_id = session.session_id.clone();

    for (minimum, expect_success) in [(2, false), (1, true)] {
        let task = tokio::spawn({
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
                            filter: Some("focused".to_string()),
                            all_targets: None,
                            all_features: None,
                            no_default_features: None,
                            features: None,
                            package: None,
                            no_run: None,
                            require_tests: None,
                            min_tests: Some(minimum),
                            timeout_secs: Some(600),
                        },
                        Some(&auth),
                    )
                    .await
            }
        });
        let (request, job_id) = poll_start_validation_job(&runtime, client_id).await;
        runtime
            .runner_registry
            .update_job(cargo_test_update(
                client_id,
                &request.request_id,
                &job_id,
                "completed",
                "running 1 test\n\ntest focused ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored\n",
                "",
                Some(0),
                completed_progress(),
                true,
            ))
            .await
            .unwrap();

        let result = task.await.unwrap();
        assert_eq!(
            result.success, expect_success,
            "minimum={minimum}: {result:?}"
        );
        assert_eq!(result.output["exit_code"], 0);
        assert_eq!(result.output["tests_run_count"], 1);
        assert_eq!(result.output["tests_failed"], 0);
        assert_eq!(
            result.output["test_count_assertion"]["reason_code"],
            if expect_success {
                "minimum_satisfied"
            } else {
                "minimum_not_met"
            }
        );
    }

    let summary = runtime.sessions.summary(&session_id, Some(50)).unwrap();
    let raw_finished = summary
        .events
        .iter()
        .filter(|event| event.kind == "tool_call_finished" && event.tool_name == "cargo_test")
        .collect::<Vec<_>>();
    assert_eq!(raw_finished.len(), 2);
    assert_eq!(raw_finished[0].status.as_deref(), Some("failed"));
    assert_eq!(raw_finished[1].status.as_deref(), Some("succeeded"));
    assert_eq!(
        raw_finished[0]
            .validation_output_summary
            .as_ref()
            .and_then(|value| value.pointer("/test_count_assertion/reason_code"))
            .and_then(|value| value.as_str()),
        Some("minimum_not_met")
    );

    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["status"], "passed");
    assert_eq!(validation["historical_failures"]["count"], 0);
    assert_eq!(validation["unresolved_failures"]["count"], 0);
    assert_eq!(validation["evidence_gaps"]["count"], 1);
    assert_eq!(validation["events"][0]["success"], false);
    assert_eq!(validation["events"][0]["validation_passed"], true);
    assert_eq!(
        validation["events"][0]["failure_class"],
        "evidence_assertion"
    );
    assert_eq!(
        validation["events"][0]["identity"],
        validation["events"][1]["identity"]
    );

    let handoff = runtime
        .dispatch_with_auth(
            ToolCall::SessionHandoffSummary {
                session_id,
                project: Some(project),
                include_workspace: Some(false),
                include_checkpoints: Some(false),
                include_validation: Some(true),
                summary_only: true,
                limit: Some(50),
            },
            Some(&auth),
        )
        .await;
    assert!(handoff.success, "{:?}", handoff.error);
    assert_eq!(handoff.output["validation"]["status"], "passed");
    assert_eq!(
        handoff.output["validation"]["current_evidence"]["status"],
        "passed"
    );
    assert_eq!(
        handoff.output["validation"]["current_evidence"]["evidence_gap_event_count"],
        1
    );
    assert_eq!(handoff.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(
        handoff.output["tool_failures"]["non_actionable_unexpected_count"],
        1
    );
    assert_eq!(
        handoff.output["tool_failures"]["actionable_unexpected_count"],
        0
    );
    assert_eq!(
        handoff.output["continuation_feedback"]["attempt"]["activity"]["failed_tool_calls"],
        1
    );
    assert_eq!(
        handoff.output["continuation_feedback"]["attempt"]["activity"]
            ["actionable_failed_tool_calls"],
        0
    );
    assert_eq!(
        handoff.output["continuation_feedback"]["attempt"]["validation"]
            ["evidence_gap_event_count"],
        1
    );
    let continuation_reasons = handoff.output["continuation_feedback"]["attempt"]["outcome"]
        ["reason_codes"]
        .as_array()
        .expect("continuation reason_codes");
    assert!(!continuation_reasons
        .iter()
        .any(|reason| reason == "actionable_failed_tool_calls"));
    assert_eq!(handoff.output["task_outcome"]["blocking"], false);
    let blocking_reasons = handoff.output["task_outcome"]["blocking_reasons"]
        .as_array()
        .expect("blocking_reasons");
    assert!(!blocking_reasons
        .iter()
        .any(|reason| reason == "unexpected_tool_failures"));
    assert!(!blocking_reasons
        .iter()
        .any(|reason| reason == "validation_failed"));
}

#[tokio::test]
async fn durable_cargo_test_explicit_zero_opt_out_survives_job_reconciliation() {
    let client_id = "vhandoff-zero-opt-out";
    let runtime = runtime_with_agent_project(client_id)
        .with_validation_sync_wait(std::time::Duration::from_millis(50));
    register_agent(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            async_shell_jobs: true,
            structured_validation_argv: true,
            structured_cargo_test_count_assertion: true,
            structured_cargo_test_execution_policy: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id(client_id);
    let auth = auth_context(None, true);
    let session = runtime.sessions.start_session(Some(project.clone()), None);
    let session_id = session.session_id.clone();

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let session_id = session_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::CargoTest {
                        project,
                        session_id: Some(session_id),
                        cwd: None,
                        filter: Some("focused".to_string()),
                        all_targets: None,
                        all_features: None,
                        no_default_features: None,
                        features: None,
                        package: None,
                        no_run: None,
                        require_tests: Some(false),
                        min_tests: None,
                        timeout_secs: Some(1800),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let (request, job_id) = poll_start_validation_job(&runtime, client_id).await;
    let durable = request
        .job_context
        .as_ref()
        .and_then(|context| context.validation.as_ref())
        .expect("durable validation metadata");
    assert_eq!(durable.minimum_tests, None);
    assert_eq!(durable.require_tests, Some(false));
    assert_eq!(durable.no_run, None);

    let handoff = task.await.unwrap();
    assert!(handoff.success, "{:?}", handoff.error);
    assert_eq!(handoff.output["promoted_to_job"], true);
    assert_eq!(handoff.output["job_id"], job_id);

    runtime
        .runner_registry
        .update_job(cargo_test_update(
            client_id,
            &request.request_id,
            &job_id,
            "completed",
            "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored\n",
            "",
            Some(0),
            completed_progress(),
            true,
        ))
        .await
        .unwrap();

    let status = runtime
        .job_status_for_auth(job_id, false, Some(&auth))
        .await;
    assert!(status.success, "{:?}", status.error);
    assert_eq!(status.output["validation"]["passed"], true);
    assert_eq!(status.output["validation"]["tests_run_count"], 0);
    assert_eq!(status.output["validation"]["zero_tests_run"], true);
    assert_eq!(status.output["validation"]["require_tests"], false);

    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(50))
        .unwrap();
    let validation = runtime
        .validation_summary_for_session_with_jobs(&summary, 50, Some(&auth))
        .await;
    assert_eq!(validation["status"], "passed");
    assert_eq!(validation["successes"], 1);
    assert_eq!(validation["latest_status"], "passed");
    assert_eq!(validation["latest_success"]["require_tests"], false);
    assert_eq!(validation["latest_success"]["zero_tests_run"], true);
}
