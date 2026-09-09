use super::*;

#[test]
fn registry_lifecycle_alias_consumes_core_contract_without_absorbing_recovery() {
    use crate::state::JobLifecycleState;
    use webcodex_core::runner_job_lifecycle::RunnerJobLifecycle;

    let runner_queued = JobLifecycleState::from_wire("agent_queued").unwrap();
    let started = JobLifecycleState::from_wire("started").unwrap();
    assert_eq!(runner_queued, RunnerJobLifecycle::RunnerQueued);
    assert!(runner_queued.is_runner_active());
    assert_eq!(started, RunnerJobLifecycle::StartedLegacy);
    assert!(started.is_active());
    assert!(!started.is_runner_active());

    assert!(JobLifecycleState::from_wire("recovering").is_err());
    assert!(crate::job_status_is_active("recovering"));
    assert!(JobLifecycleState::from_wire("mystery").is_err());
    assert!(!crate::job_status_is_active("mystery"));
}

#[tokio::test]
async fn terminal_observed_poll_complete_and_log() {
    let registry = RunnerRegistry::default();
    registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "oe".to_string(),
            runner_instance_id: "inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: None,
            hostname: None,
            host_context: None,
            capabilities: async_job_capabilities(),
            policy: None,
        })
        .await
        .unwrap();
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: Some("/tmp".to_string()),
                command: Some("printf hello".to_string()),
                timeout_secs: Some(10),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: Some(ShellJobCodexMetadata {
                    project: Some("demo".to_string()),
                    goal_id: Some("goal-1".to_string()),
                    client_request_id: Some("crid-1".to_string()),
                    command: Some("printf hello".to_string()),
                    kind: Some("command".to_string()),
                    suite: None,
                    script_path: None,
                    reason: Some("test job".to_string()),
                    max_runtime_secs: Some(10),
                }),
            },
            "test".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(job.status, "queued");
    assert_eq!(
        job.codex
            .as_ref()
            .and_then(|codex| codex.client_request_id.as_deref()),
        Some("crid-1")
    );
    let polled = registry
        .poll(RunnerPollRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst".to_string(),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(polled.command, "printf hello");
    let running = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(running.status, "agent_queued");
    registry
        .complete(RunnerResultRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst".to_string(),
            request_id: polled.request_id,
            exit_code: Some(0),
            stdout: Some("hello\n".to_string()),
            stderr: Some(String::new()),
            duration_ms: Some(20),
            error: None,
        })
        .await
        .unwrap();
    let done = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(done.status, "completed");
    assert_eq!(done.exit_code, Some(0));
    {
        let inner = registry.inner.lock().await;
        let record = inner.jobs_by_id.get(&job.job_id).unwrap();
        assert!(record.observation.terminal_observed_at.is_some());
        assert_eq!(record.observation.terminal_observed_at, record.ended_at);
    }
    assert_eq!(
        done.codex
            .as_ref()
            .and_then(|codex| codex.project.as_deref()),
        Some("demo")
    );
    let listed = registry.list_jobs(Some(10)).await;
    assert_eq!(
        listed
            .iter()
            .find(|listed| listed.job_id == job.job_id)
            .and_then(|listed| listed.codex.as_ref())
            .and_then(|codex| codex.goal_id.as_deref()),
        Some("goal-1")
    );
    let (_info, stdout, stderr, next_stdout, next_stderr) = registry
        .job_log(&job.job_id, Some(1), Some(1), None)
        .await
        .unwrap();
    assert_eq!(stdout.as_deref(), Some("hello\n"));
    assert_eq!(stderr.as_deref(), Some(""));
    assert_eq!(next_stdout, 2);
    assert_eq!(next_stderr, 1);
}

#[tokio::test]
async fn job_update_rejects_mismatched_request_id_without_mutating_target_job() {
    let registry = RunnerRegistry::default();
    registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "oe".to_string(),
            runner_instance_id: "inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: None,
            hostname: None,
            host_context: None,
            capabilities: async_job_capabilities(),
            policy: None,
        })
        .await
        .unwrap();

    let start = |command: &str| ShellJobOpRequest {
        op: "start".to_string(),
        client_id: Some("oe".to_string()),
        cwd: None,
        command: Some(command.to_string()),
        timeout_secs: Some(10),
        job_id: None,
        since_stdout_line: None,
        since_stderr_line: None,
        tail_lines: None,
        limit: None,
        codex: None,
    };
    let job_a = registry
        .start_job(start("job-a"), "test".to_string())
        .await
        .unwrap();
    let job_b = registry
        .start_job(start("job-b"), "test".to_string())
        .await
        .unwrap();
    let request_a = job_a.request_id.clone().expect("job A request id");
    let request_b = job_b.request_id.clone().expect("job B request id");

    let update = |request_id: String| RunnerJobUpdateRequest {
        client_id: "oe".to_string(),
        runner_instance_id: "inst".to_string(),
        job_id: job_b.job_id.clone(),
        request_id: Some(request_id),
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
    };

    let error = registry.update_job(update(request_a)).await.unwrap_err();
    assert_eq!(error, "job update request_id does not match job_id");
    assert_eq!(
        registry.get_job(&job_b.job_id).await.unwrap().status,
        "queued"
    );

    let mut invalid = update(request_b.clone());
    invalid.status = "mystery".to_string();
    let error = registry.update_job(invalid).await.unwrap_err();
    assert!(error.contains("job update status 'mystery' is invalid"));
    assert_eq!(
        registry.get_job(&job_b.job_id).await.unwrap().status,
        "queued",
        "unknown legacy wire status must fail closed without mutating the job"
    );

    let accepted = registry.update_job(update(request_b)).await.unwrap();
    assert_eq!(accepted.status, "running");
}

#[tokio::test]
async fn terminal_observed_queued_stop_records_server_time() {
    let registry = RunnerRegistry::default();
    registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "oe".to_string(),
            runner_instance_id: "inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: None,
            hostname: None,
            host_context: None,
            capabilities: async_job_capabilities(),
            policy: None,
        })
        .await
        .unwrap();
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: None,
                command: Some("sleep 10".to_string()),
                timeout_secs: Some(10),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "test".to_string(),
        )
        .await
        .unwrap();
    let stopped = registry
        .stop_job(&job.job_id, "test".to_string())
        .await
        .unwrap();
    assert_eq!(stopped.status, "stopped");
    {
        let inner = registry.inner.lock().await;
        let record = inner.jobs_by_id.get(&job.job_id).unwrap();
        assert!(record.observation.terminal_observed_at.is_some());
        assert_eq!(record.observation.terminal_observed_at, record.ended_at);
    }
    let polled = registry
        .poll(RunnerPollRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst".to_string(),
        })
        .await
        .unwrap();
    assert!(polled.is_none());
}

#[tokio::test]
async fn registry_shell_job_stop_running_delivers_stop_to_client() {
    let registry = RunnerRegistry::default();
    registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "oe".to_string(),
            runner_instance_id: "inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: None,
            hostname: None,
            host_context: None,
            capabilities: async_job_capabilities(),
            policy: None,
        })
        .await
        .unwrap();
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: None,
                command: Some("sleep 10".to_string()),
                timeout_secs: Some(10),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "test".to_string(),
        )
        .await
        .unwrap();
    let started = registry
        .poll(RunnerPollRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst".to_string(),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(started.kind, "start_job");

    let stop_requested = registry
        .stop_job(&job.job_id, "test".to_string())
        .await
        .unwrap();
    assert_eq!(stop_requested.status, "stop_requested");
    let duplicate = registry
        .stop_job(&job.job_id, "test".to_string())
        .await
        .unwrap();
    assert_eq!(duplicate.status, "stop_requested");
    let stop = registry
        .poll(RunnerPollRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst".to_string(),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stop.kind, "stop_job");
    assert_eq!(stop.job_id.as_deref(), Some(job.job_id.as_str()));
    assert!(
        registry
            .poll(RunnerPollRequest {
                client_id: "oe".to_string(),
                runner_instance_id: "inst".to_string(),
            })
            .await
            .unwrap()
            .is_none(),
        "duplicate stop must not enqueue a second Runner operation"
    );
}

#[tokio::test]
async fn registry_marks_running_job_lost_when_client_stale() {
    let registry = RunnerRegistry::default();
    registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "oe".to_string(),
            runner_instance_id: "inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: None,
            hostname: None,
            host_context: None,
            capabilities: async_job_capabilities(),
            policy: None,
        })
        .await
        .unwrap();
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: None,
                command: Some("sleep 10".to_string()),
                timeout_secs: Some(10),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "test".to_string(),
        )
        .await
        .unwrap();
    let _ = registry
        .poll(RunnerPollRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst".to_string(),
        })
        .await
        .unwrap()
        .unwrap();
    {
        let mut inner = registry.inner.lock().await;
        let client = inner.runners.get_mut("oe").unwrap();
        client.last_seen = now_ts() - RUNNER_ONLINE_WINDOW_SECS - 1;
    }
    let lost = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(lost.status, "lost");
    assert!(lost.error.unwrap().contains("stale"));
}
