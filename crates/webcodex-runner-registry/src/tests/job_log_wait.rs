use super::*;

// ============================================================================
// Bounded `job_log`/`job_tail` waits
// ============================================================================
//
// These tests drive `job_log_for_auth` with `after_observation_token`/`wait_secs`
// against a sequenced Runner-backed job. A sequenced Runner (`job_state_reconciliation`)
// advances `last_update_seq` via `update_job`, so waiters observe real sequence
// advancement, log growth, and terminal transitions.

fn sequenced_job_capabilities() -> RunnerCapabilities {
    RunnerCapabilities {
        async_jobs: true,
        async_shell_jobs: true,
        jobs: true,
        job_state_reconciliation: true,
        coding_agent_runs: false,
        structured_validation_argv: true,
        ..Default::default()
    }
}

fn wait_job_update(
    instance: &str,
    job_id: &str,
    sequence: u64,
    status: &str,
    stdout_chunk: Option<&str>,
    finished: bool,
) -> RunnerJobUpdateRequest {
    RunnerJobUpdateRequest {
        client_id: "oe".to_string(),
        runner_instance_id: instance.to_string(),
        job_id: job_id.to_string(),
        request_id: None,
        update_seq: Some(sequence),
        status: status.to_string(),
        stdout_chunk: stdout_chunk.map(str::to_string),
        stderr_chunk: None,
        stdout_tail: None,
        stderr_tail: None,
        log_snapshot: None,
        exit_code: finished.then_some(0),
        duration_ms: finished.then_some(2_000),
        error: None,
        command_execution_state: None,
        validation_progress: None,
        activity: None,
        finished,
    }
}

async fn register_sequenced(registry: &RunnerRegistry, instance: &str) {
    registry
        .register(current_runner_registration(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: Some(crate::runner_protocol::ShellJobInventory {
                active_complete: true,
                jobs: Vec::new(),
            }),
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "oe".to_string(),
            runner_instance_id: instance.to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            host_context: None,
            capabilities: sequenced_job_capabilities(),
            policy: None,
        }))
        .await
        .unwrap();
}

async fn start_wait_job(registry: &RunnerRegistry) -> crate::runner_protocol::ShellJobInfo {
    register_sequenced(registry, "inst-wait").await;
    registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: Some("/tmp".to_string()),
                command: Some("printf 'line one\\n'".to_string()),
                timeout_secs: Some(30),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "tester".to_string(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn job_log_wait_observation_update_between_calls_is_immediate() {
    let registry = RunnerRegistry::default();
    let job = start_wait_job(&registry).await;
    let token0 = job.observation_token.clone().unwrap();
    registry
        .update_job(wait_job_update(
            "inst-wait",
            &job.job_id,
            1,
            "running",
            Some("hi\n"),
            false,
        ))
        .await
        .unwrap();
    let started = tokio::time::Instant::now();
    let (info, stdout, _, _, _, wait) = registry
        .job_log_for_auth(None, &job.job_id, None, None, None, Some(&token0), Some(5))
        .await
        .unwrap();
    assert_eq!(wait.wait_outcome, JobLogWaitOutcome::Immediate);
    assert!(wait.changed);
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert_eq!(stdout.as_deref(), Some("hi\n"));
    assert_ne!(info.observation_token.as_deref(), Some(token0.as_str()));
    assert_eq!(info.last_update_seq, Some(1));
}

#[tokio::test]
async fn job_log_wait_server_transition_between_calls_is_immediate() {
    let registry = RunnerRegistry::default();
    let job = start_wait_job(&registry).await;
    registry
        .update_job(wait_job_update(
            "inst-wait",
            &job.job_id,
            1,
            "running",
            None,
            false,
        ))
        .await
        .unwrap();
    let token0 = registry
        .get_job(&job.job_id)
        .await
        .unwrap()
        .observation_token
        .unwrap();
    registry
        .stop_job(&job.job_id, "tester".to_string())
        .await
        .unwrap();
    let (info, _, _, _, _, wait) = registry
        .job_log_for_auth(None, &job.job_id, None, None, None, Some(&token0), Some(5))
        .await
        .unwrap();
    assert_eq!(wait.wait_outcome, JobLogWaitOutcome::Immediate);
    assert!(wait.changed);
    assert_eq!(info.status, "stop_requested");
}

#[tokio::test]
async fn job_log_wait_epoch_mismatch_refreshes_immediately() {
    let registry = RunnerRegistry::default();
    let job = start_wait_job(&registry).await;
    let (baseline, _, _, _, _, _) = registry
        .job_log_for_auth(None, &job.job_id, None, None, Some(10), None, None)
        .await
        .unwrap();
    let parsed = crate::job_observation::JobObservationToken::parse(
        baseline.observation_token.as_deref().unwrap(),
    )
    .unwrap();
    let stale = crate::job_observation::JobObservationToken::new(
        job.job_id.clone(),
        "ffffffffffffffffffffffffffffffff",
        parsed.revision,
        parsed.stdout_cursor.unwrap(),
        parsed.stderr_cursor.unwrap(),
    )
    .unwrap()
    .encode();
    let started = tokio::time::Instant::now();
    let (_, _, _, _, _, wait) = registry
        .job_log_for_auth(None, &job.job_id, None, None, None, Some(&stale), Some(5))
        .await
        .unwrap();
    assert_eq!(wait.wait_outcome, JobLogWaitOutcome::Immediate);
    assert!(wait.changed);
    assert_eq!(
        wait.log_delta_status,
        crate::job_observation::JobLogDeltaStatus::Reset
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}

#[tokio::test]
async fn job_log_wait_rejects_wrong_job_malformed_and_oversized_tokens() {
    let registry = RunnerRegistry::default();
    let first = start_wait_job(&registry).await;
    let second = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".into(),
                client_id: Some("oe".into()),
                cwd: Some("/tmp".into()),
                command: Some("true".into()),
                timeout_secs: Some(30),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "tester".into(),
        )
        .await
        .unwrap();
    let wrong = registry
        .job_log_for_auth(
            None,
            &second.job_id,
            None,
            None,
            None,
            first.observation_token.as_deref(),
            Some(1),
        )
        .await
        .unwrap_err();
    assert!(wrong.contains("different Job"));
    let malformed = registry
        .job_log_for_auth(None, &first.job_id, None, None, None, Some("bad"), Some(1))
        .await
        .unwrap_err();
    assert!(malformed.contains("malformed"));
    let oversized = "x".repeat(crate::job_observation::MAX_JOB_OBSERVATION_TOKEN_LEN + 1);
    let oversized = registry
        .job_log_for_auth(
            None,
            &first.job_id,
            None,
            None,
            None,
            Some(&oversized),
            Some(1),
        )
        .await
        .unwrap_err();
    assert!(oversized.contains("exceeds 192"));
}

#[tokio::test]
async fn job_log_wait_same_token_times_out_without_changed() {
    let registry = RunnerRegistry::default();
    let job = start_wait_job(&registry).await;
    let token = job.observation_token.clone().unwrap();
    let (_, _, _, _, _, wait) = registry
        .job_log_for_auth(None, &job.job_id, None, None, None, Some(&token), Some(1))
        .await
        .unwrap();
    assert_eq!(wait.wait_outcome, JobLogWaitOutcome::Timeout);
    assert!(!wait.changed);
    assert!(!wait.terminal);
}

#[tokio::test]
async fn job_log_wait_wakes_all_waiters_without_holding_registry_mutex() {
    let registry = RunnerRegistry::default();
    let job = start_wait_job(&registry).await;
    let token = job.observation_token.clone().unwrap();
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let registry = registry.clone();
        let job_id = job.job_id.clone();
        let token = token.clone();
        tasks.push(tokio::spawn(async move {
            registry
                .job_log_for_auth(None, &job_id, None, None, None, Some(&token), Some(5))
                .await
                .unwrap()
                .5
        }));
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    registry
        .update_job(wait_job_update(
            "inst-wait",
            &job.job_id,
            1,
            "running",
            Some("one\n"),
            false,
        ))
        .await
        .unwrap();
    for task in tasks {
        let wait = task.await.unwrap();
        assert_eq!(wait.wait_outcome, JobLogWaitOutcome::Updated);
        assert!(wait.changed);
    }
}

#[tokio::test]
async fn job_log_wait_stale_or_replayed_sequence_does_not_change_token() {
    let registry = RunnerRegistry::default();
    let job = start_wait_job(&registry).await;
    registry
        .update_job(wait_job_update(
            "inst-wait",
            &job.job_id,
            1,
            "running",
            Some("one\n"),
            false,
        ))
        .await
        .unwrap();
    let token = registry
        .get_job(&job.job_id)
        .await
        .unwrap()
        .observation_token
        .unwrap();
    registry
        .update_job(wait_job_update(
            "inst-wait",
            &job.job_id,
            1,
            "running",
            Some("replay\n"),
            false,
        ))
        .await
        .unwrap();
    let current = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(current.observation_token.as_deref(), Some(token.as_str()));
    assert_eq!(current.last_update_seq, Some(1));
}

#[tokio::test]
async fn job_log_wait_sequenced_update_changes_token_even_when_tail_is_same() {
    let registry = RunnerRegistry::default();
    let job = start_wait_job(&registry).await;
    registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "oe".into(),
            runner_instance_id: "inst-wait".into(),
            update_seq: Some(1),
            job_id: job.job_id.clone(),
            request_id: None,
            status: "running".into(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: Some("same\n".into()),
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
    let token = registry
        .get_job(&job.job_id)
        .await
        .unwrap()
        .observation_token
        .unwrap();
    registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "oe".into(),
            runner_instance_id: "inst-wait".into(),
            update_seq: Some(2),
            job_id: job.job_id.clone(),
            request_id: None,
            status: "running".into(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: Some("same\n".into()),
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
    let current = registry.get_job(&job.job_id).await.unwrap();
    assert_ne!(
        current.observation_token.as_deref(),
        Some(token.as_str()),
        "Runner sequence itself remains a public diagnostic field and changed"
    );
}

#[tokio::test]
async fn job_log_wait_recovery_transition_between_calls_is_immediate() {
    let registry = RunnerRegistry::default();
    let job = start_wait_job(&registry).await;
    let mut running = wait_job_update("inst-wait", &job.job_id, 1, "running", None, false);
    running.activity = Some(crate::runner_protocol::ShellJobActivity {
        state: crate::runner_protocol::ShellJobActivityState::Working,
        phase: crate::runner_protocol::ShellJobActivityPhase::ProcessRunning,
        source: crate::runner_protocol::ShellJobActivitySource::RunnerExecution,
    });
    registry.update_job(running).await.unwrap();
    let token = registry
        .get_job(&job.job_id)
        .await
        .unwrap()
        .observation_token
        .unwrap();
    registry.reconcile_disconnect("oe", "inst-wait").await;
    let (info, _, _, _, _, wait) = registry
        .job_log_for_auth(None, &job.job_id, None, None, None, Some(&token), Some(5))
        .await
        .unwrap();
    assert_eq!(wait.wait_outcome, JobLogWaitOutcome::Immediate);
    assert!(wait.changed);
    assert_eq!(info.status, "recovering");
    assert_eq!(info.recovery_state.as_deref(), Some("recovering"));
    assert_eq!(info.activity, None);
}

#[tokio::test]
async fn job_log_wait_legacy_update_between_calls_and_noop_replacement() {
    let registry = RunnerRegistry::default();
    let capabilities = RunnerCapabilities {
        async_jobs: true,
        async_shell_jobs: true,
        jobs: true,
        ..Default::default()
    };
    registry
        .register(current_runner_registration(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "legacy".to_string(),
            runner_instance_id: "legacy-inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            host_context: None,
            capabilities: capabilities,
            policy: None,
        }))
        .await
        .unwrap();
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("legacy".to_string()),
                cwd: Some("/tmp".to_string()),
                command: Some("printf legacy".to_string()),
                timeout_secs: Some(30),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "tester".to_string(),
        )
        .await
        .unwrap();
    let token0 = job.observation_token.clone().unwrap();
    let update = || RunnerJobUpdateRequest {
        client_id: "legacy".to_string(),
        runner_instance_id: "legacy-inst".to_string(),
        update_seq: None,
        job_id: job.job_id.clone(),
        request_id: None,
        status: "running".to_string(),
        stdout_chunk: None,
        stderr_chunk: None,
        stdout_tail: Some("legacy output\n".to_string()),
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
    registry.update_job(update()).await.unwrap();
    let (info, stdout, _, _, _, wait) = registry
        .job_log_for_auth(None, &job.job_id, None, None, None, Some(&token0), Some(5))
        .await
        .unwrap();
    assert_eq!(wait.wait_outcome, JobLogWaitOutcome::Immediate);
    assert!(wait.changed);
    assert_eq!(stdout.as_deref(), Some("legacy output\n"));
    let token1 = info.observation_token.unwrap();
    registry.update_job(update()).await.unwrap();
    let current = registry.get_job(&job.job_id).await.unwrap();
    let response_token = crate::job_observation::JobObservationToken::parse(&token1).unwrap();
    let lifecycle_token = crate::job_observation::JobObservationToken::parse(
        current.observation_token.as_deref().unwrap(),
    )
    .unwrap();
    assert_eq!(response_token.epoch, lifecycle_token.epoch);
    assert_eq!(response_token.revision, lifecycle_token.revision);
    assert_eq!(response_token.stdout_cursor, Some(2));
    assert_eq!(response_token.stderr_cursor, Some(1));
    assert_eq!(current.last_update_seq, Some(0));
    assert!(
        current.activity.is_none(),
        "older Runner updates may omit activity"
    );
}

#[tokio::test]
async fn job_log_wait_activity_only_legacy_transition_advances_revision_and_wakes_waiter() {
    let registry = RunnerRegistry::default();
    let capabilities = RunnerCapabilities {
        async_jobs: true,
        async_shell_jobs: true,
        jobs: true,
        ..Default::default()
    };
    registry
        .register(current_runner_registration(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "activity-legacy".to_string(),
            runner_instance_id: "activity-legacy-inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            host_context: None,
            capabilities,
            policy: None,
        }))
        .await
        .unwrap();
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".into(),
                client_id: Some("activity-legacy".into()),
                cwd: Some("/tmp".into()),
                command: Some("sleep 10".into()),
                timeout_secs: Some(30),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "tester".into(),
        )
        .await
        .unwrap();
    let base_update = || RunnerJobUpdateRequest {
        client_id: "activity-legacy".into(),
        runner_instance_id: "activity-legacy-inst".into(),
        update_seq: None,
        job_id: job.job_id.clone(),
        request_id: None,
        status: "running".into(),
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
    registry.update_job(base_update()).await.unwrap();
    let baseline = registry.get_job(&job.job_id).await.unwrap();
    let token = baseline.observation_token.clone().unwrap();
    let revision_before = {
        let inner = registry.inner.lock().await;
        inner
            .jobs_by_id
            .get(&job.job_id)
            .unwrap()
            .observation
            .revision
            .load(std::sync::atomic::Ordering::Relaxed)
    };

    let waiting_registry = registry.clone();
    let waiting_job_id = job.job_id.clone();
    let waiting_token = token.clone();
    let waiter = tokio::spawn(async move {
        waiting_registry
            .job_log_for_auth(
                None,
                &waiting_job_id,
                None,
                None,
                None,
                Some(&waiting_token),
                Some(5),
            )
            .await
            .unwrap()
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let mut activity_update = base_update();
    activity_update.activity = Some(crate::runner_protocol::ShellJobActivity {
        state: crate::runner_protocol::ShellJobActivityState::Working,
        phase: crate::runner_protocol::ShellJobActivityPhase::ProcessRunning,
        source: crate::runner_protocol::ShellJobActivitySource::RunnerExecution,
    });
    registry.update_job(activity_update).await.unwrap();

    let (observed, stdout, stderr, _, _, wait) = waiter.await.unwrap();
    assert_eq!(wait.wait_outcome, JobLogWaitOutcome::Updated);
    assert!(wait.changed);
    assert_eq!(stdout.as_deref(), Some(""));
    assert_eq!(stderr.as_deref(), Some(""));
    assert_eq!(
        observed.activity,
        Some(crate::runner_protocol::ShellJobActivity {
            state: crate::runner_protocol::ShellJobActivityState::Working,
            phase: crate::runner_protocol::ShellJobActivityPhase::ProcessRunning,
            source: crate::runner_protocol::ShellJobActivitySource::RunnerExecution,
        })
    );
    let revision_after = {
        let inner = registry.inner.lock().await;
        inner
            .jobs_by_id
            .get(&job.job_id)
            .unwrap()
            .observation
            .revision
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    assert!(revision_after > revision_before);
}

#[tokio::test]
async fn noncanonical_activity_fails_job_closed_without_retaining_activity() {
    let registry = RunnerRegistry::default();
    let job = start_wait_job(&registry).await;
    let mut update = wait_job_update("inst-wait", &job.job_id, 1, "running", None, false);
    update.activity = Some(crate::runner_protocol::ShellJobActivity {
        state: crate::runner_protocol::ShellJobActivityState::Waiting,
        phase: crate::runner_protocol::ShellJobActivityPhase::ProcessRunning,
        source: crate::runner_protocol::ShellJobActivitySource::RunnerExecution,
    });

    let failed = registry.update_job(update).await.unwrap();
    assert_eq!(failed.status, "failed");
    assert_eq!(failed.activity, None);
    assert!(failed
        .error
        .as_deref()
        .is_some_and(|error| error.contains("job_activity_invalid")));
}

#[tokio::test]
async fn agent_job_log_observation_is_baseline_then_independent_deltas() {
    let registry = RunnerRegistry::default();
    let job = start_wait_job(&registry).await;
    let stdout = (1..=10)
        .map(|line| format!("stdout {line}\n"))
        .collect::<String>();
    registry
        .update_job(wait_job_update(
            "inst-wait",
            &job.job_id,
            1,
            "running",
            Some(&stdout),
            false,
        ))
        .await
        .unwrap();

    let (baseline_job, stdout, stderr, _, _, baseline) = registry
        .job_log_for_auth(None, &job.job_id, None, None, Some(20), None, None)
        .await
        .unwrap();
    assert_eq!(
        baseline.log_delta_status,
        crate::job_observation::JobLogDeltaStatus::Baseline
    );
    assert!(stdout.as_deref().unwrap().starts_with("stdout 1\n"));
    assert_eq!(stderr.as_deref(), Some(""));
    let token0 = baseline_job.observation_token.unwrap();
    let parsed0 = crate::job_observation::JobObservationToken::parse(&token0).unwrap();
    assert_eq!(parsed0.stdout_cursor, Some(11));
    assert_eq!(parsed0.stderr_cursor, Some(1));

    let (same_job, stdout, stderr, _, _, unchanged) = registry
        .job_log_for_auth(None, &job.job_id, None, None, Some(20), Some(&token0), None)
        .await
        .unwrap();
    assert!(!unchanged.changed);
    assert_eq!(
        unchanged.log_delta_status,
        crate::job_observation::JobLogDeltaStatus::Unchanged
    );
    assert_eq!(stdout.as_deref(), Some(""));
    assert_eq!(stderr.as_deref(), Some(""));
    assert_eq!(same_job.observation_token.as_deref(), Some(token0.as_str()));

    registry
        .update_job(wait_job_update(
            "inst-wait",
            &job.job_id,
            2,
            "running",
            Some("stdout 11\nstdout 12\nstdout 13\n"),
            false,
        ))
        .await
        .unwrap();
    let (stdout_job, stdout, stderr, _, _, stdout_delta) = registry
        .job_log_for_auth(None, &job.job_id, None, None, Some(20), Some(&token0), None)
        .await
        .unwrap();
    assert!(stdout_delta.changed);
    assert_eq!(
        stdout_delta.log_delta_status,
        crate::job_observation::JobLogDeltaStatus::Delta
    );
    assert_eq!(stdout.as_deref(), Some("stdout 11\nstdout 12\nstdout 13\n"));
    assert_eq!(stderr.as_deref(), Some(""));
    let token1 = stdout_job.observation_token.unwrap();

    registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "oe".into(),
            runner_instance_id: "inst-wait".into(),
            job_id: job.job_id.clone(),
            request_id: None,
            update_seq: Some(3),
            status: "running".into(),
            stdout_chunk: None,
            stderr_chunk: Some("stderr 1\nstderr 2\n".into()),
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
    let (stderr_job, stdout, stderr, _, _, stderr_delta) = registry
        .job_log_for_auth(None, &job.job_id, None, None, Some(20), Some(&token1), None)
        .await
        .unwrap();
    assert_eq!(
        stderr_delta.log_delta_status,
        crate::job_observation::JobLogDeltaStatus::Delta
    );
    assert_eq!(stdout.as_deref(), Some(""));
    assert_eq!(stderr.as_deref(), Some("stderr 1\nstderr 2\n"));
    let token2 = stderr_job.observation_token.unwrap();

    registry
        .update_job(wait_job_update(
            "inst-wait",
            &job.job_id,
            4,
            "running",
            None,
            false,
        ))
        .await
        .unwrap();
    let (lifecycle_job, stdout, stderr, _, _, lifecycle) = registry
        .job_log_for_auth(None, &job.job_id, None, None, Some(20), Some(&token2), None)
        .await
        .unwrap();
    assert!(lifecycle.changed);
    assert_eq!(
        lifecycle.log_delta_status,
        crate::job_observation::JobLogDeltaStatus::Unchanged
    );
    assert_eq!(stdout.as_deref(), Some(""));
    assert_eq!(stderr.as_deref(), Some(""));
    let token3 = lifecycle_job.observation_token.unwrap();

    registry
        .update_job(wait_job_update(
            "inst-wait",
            &job.job_id,
            5,
            "completed",
            None,
            true,
        ))
        .await
        .unwrap();
    let (terminal_job, stdout, stderr, _, _, terminal) = registry
        .job_log_for_auth(None, &job.job_id, None, None, Some(20), Some(&token3), None)
        .await
        .unwrap();
    assert!(terminal.changed);
    assert!(terminal.terminal);
    assert_eq!(terminal_job.status, "completed");
    assert_eq!(terminal_job.exit_code, Some(0));
    assert_eq!(
        terminal.log_delta_status,
        crate::job_observation::JobLogDeltaStatus::Unchanged
    );
    assert_eq!(stdout.as_deref(), Some(""));
    assert_eq!(stderr.as_deref(), Some(""));
}

#[tokio::test]
async fn agent_job_log_replays_partial_lines_until_each_stream_completes() {
    let registry = RunnerRegistry::default();
    let job = start_wait_job(&registry).await;
    registry
        .update_job(wait_job_update(
            "inst-wait",
            &job.job_id,
            1,
            "running",
            Some("abc"),
            false,
        ))
        .await
        .unwrap();

    let (baseline_job, stdout, stderr, _, _, baseline) = registry
        .job_log_for_auth(None, &job.job_id, None, None, Some(10), None, None)
        .await
        .unwrap();
    assert_eq!(
        baseline.log_delta_status,
        crate::job_observation::JobLogDeltaStatus::Baseline
    );
    assert_eq!(stdout.as_deref(), Some("abc\n"));
    assert_eq!(stderr.as_deref(), Some(""));
    let token0 = baseline_job.observation_token.unwrap();
    let parsed0 = crate::job_observation::JobObservationToken::parse(&token0).unwrap();
    assert_eq!(parsed0.stdout_cursor, Some(1));
    assert_eq!(parsed0.stderr_cursor, Some(1));

    registry
        .update_job(wait_job_update(
            "inst-wait",
            &job.job_id,
            2,
            "running",
            Some("def"),
            false,
        ))
        .await
        .unwrap();
    let (first_job, stdout, _, _, _, first_growth) = registry
        .job_log_for_auth(None, &job.job_id, None, None, Some(10), Some(&token0), None)
        .await
        .unwrap();
    assert_eq!(
        first_growth.log_delta_status,
        crate::job_observation::JobLogDeltaStatus::Delta
    );
    assert_eq!(stdout.as_deref(), Some("abcdef\n"));
    let token1 = first_job.observation_token.unwrap();
    assert_eq!(
        crate::job_observation::JobObservationToken::parse(&token1)
            .unwrap()
            .stdout_cursor,
        Some(1)
    );

    registry
        .update_job(wait_job_update(
            "inst-wait",
            &job.job_id,
            3,
            "running",
            Some("ghi"),
            false,
        ))
        .await
        .unwrap();
    let (second_job, stdout, _, _, _, second_growth) = registry
        .job_log_for_auth(None, &job.job_id, None, None, Some(10), Some(&token1), None)
        .await
        .unwrap();
    assert_eq!(
        second_growth.log_delta_status,
        crate::job_observation::JobLogDeltaStatus::Delta
    );
    assert_eq!(stdout.as_deref(), Some("abcdefghi\n"));
    let token2 = second_job.observation_token.unwrap();
    assert_eq!(
        crate::job_observation::JobObservationToken::parse(&token2)
            .unwrap()
            .stdout_cursor,
        Some(1)
    );

    registry
        .update_job(wait_job_update(
            "inst-wait",
            &job.job_id,
            4,
            "running",
            Some("\n"),
            false,
        ))
        .await
        .unwrap();
    let (completed_stdout_job, stdout, _, _, _, completed_stdout) = registry
        .job_log_for_auth(None, &job.job_id, None, None, Some(10), Some(&token2), None)
        .await
        .unwrap();
    assert_eq!(
        completed_stdout.log_delta_status,
        crate::job_observation::JobLogDeltaStatus::Delta
    );
    assert_eq!(stdout.as_deref(), Some("abcdefghi\n"));
    let token3 = completed_stdout_job.observation_token.unwrap();
    let parsed3 = crate::job_observation::JobObservationToken::parse(&token3).unwrap();
    assert_eq!(parsed3.stdout_cursor, Some(2));
    assert_eq!(parsed3.stderr_cursor, Some(1));

    registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "oe".into(),
            runner_instance_id: "inst-wait".into(),
            job_id: job.job_id.clone(),
            request_id: None,
            update_seq: Some(5),
            status: "running".into(),
            stdout_chunk: None,
            stderr_chunk: Some("err".into()),
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
    let (stderr_job, stdout, stderr, _, _, stderr_growth) = registry
        .job_log_for_auth(None, &job.job_id, None, None, Some(10), Some(&token3), None)
        .await
        .unwrap();
    assert_eq!(
        stderr_growth.log_delta_status,
        crate::job_observation::JobLogDeltaStatus::Delta
    );
    assert_eq!(stdout.as_deref(), Some(""));
    assert_eq!(stderr.as_deref(), Some("err\n"));
    let token4 = stderr_job.observation_token.unwrap();
    let parsed4 = crate::job_observation::JobObservationToken::parse(&token4).unwrap();
    assert_eq!(parsed4.stdout_cursor, Some(2));
    assert_eq!(parsed4.stderr_cursor, Some(1));

    registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "oe".into(),
            runner_instance_id: "inst-wait".into(),
            job_id: job.job_id.clone(),
            request_id: None,
            update_seq: Some(6),
            status: "completed".into(),
            stdout_chunk: None,
            stderr_chunk: Some("or\n".into()),
            stdout_tail: None,
            stderr_tail: None,
            log_snapshot: None,
            exit_code: Some(0),
            duration_ms: Some(2_000),
            error: None,
            command_execution_state: None,
            validation_progress: None,
            activity: None,
            finished: true,
        })
        .await
        .unwrap();
    let (terminal_job, stdout, stderr, _, _, terminal) = registry
        .job_log_for_auth(None, &job.job_id, None, None, Some(10), Some(&token4), None)
        .await
        .unwrap();
    assert!(terminal.terminal);
    assert_eq!(terminal_job.status, "completed");
    assert_eq!(
        terminal.log_delta_status,
        crate::job_observation::JobLogDeltaStatus::Delta
    );
    assert_eq!(stdout.as_deref(), Some(""));
    assert_eq!(stderr.as_deref(), Some("error\n"));
    let terminal_token = terminal_job.observation_token.unwrap();
    let parsed_terminal =
        crate::job_observation::JobObservationToken::parse(&terminal_token).unwrap();
    assert_eq!(parsed_terminal.stdout_cursor, Some(2));
    assert_eq!(parsed_terminal.stderr_cursor, Some(2));
}

#[tokio::test]
async fn agent_job_log_resets_when_retention_advances_past_token_cursor() {
    let registry = RunnerRegistry::default();
    let job = start_wait_job(&registry).await;
    registry
        .update_job(wait_job_update(
            "inst-wait",
            &job.job_id,
            1,
            "running",
            Some("first\n"),
            false,
        ))
        .await
        .unwrap();
    let (baseline_job, _, _, _, _, _) = registry
        .job_log_for_auth(None, &job.job_id, None, None, Some(10), None, None)
        .await
        .unwrap();
    let token = baseline_job.observation_token.unwrap();
    let large = "discarded line\n".repeat(30_000);
    registry
        .update_job(wait_job_update(
            "inst-wait",
            &job.job_id,
            2,
            "running",
            Some(&large),
            false,
        ))
        .await
        .unwrap();
    let (current, stdout, _, _, _, reset) = registry
        .job_log_for_auth(None, &job.job_id, None, None, Some(3), Some(&token), None)
        .await
        .unwrap();
    assert_eq!(
        reset.log_delta_status,
        crate::job_observation::JobLogDeltaStatus::Reset
    );
    assert!(reset.stdout_delta_reset);
    assert!(reset.stdout_truncated);
    assert_eq!(stdout.as_deref().unwrap().lines().count(), 3);
    assert!(current.stdout_retained_from_line.unwrap() > 2);
    assert!(current.stdout_log_truncated);
}

#[tokio::test]
async fn agent_job_log_bounded_wait_uses_v2_delta_and_timeout_is_empty() {
    let registry = RunnerRegistry::default();
    let job = start_wait_job(&registry).await;
    let (baseline_job, _, _, _, _, _) = registry
        .job_log_for_auth(None, &job.job_id, None, None, Some(10), None, None)
        .await
        .unwrap();
    let token0 = baseline_job.observation_token.unwrap();
    let task = tokio::spawn({
        let registry = registry.clone();
        let job_id = job.job_id.clone();
        let token0 = token0.clone();
        async move {
            registry
                .job_log_for_auth(None, &job_id, None, None, Some(10), Some(&token0), Some(5))
                .await
                .unwrap()
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    registry
        .update_job(wait_job_update(
            "inst-wait",
            &job.job_id,
            1,
            "running",
            Some("new\n"),
            false,
        ))
        .await
        .unwrap();
    let (updated_job, stdout, stderr, _, _, updated) = task.await.unwrap();
    assert_eq!(updated.wait_outcome, JobLogWaitOutcome::Updated);
    assert_eq!(
        updated.log_delta_status,
        crate::job_observation::JobLogDeltaStatus::Delta
    );
    assert_eq!(stdout.as_deref(), Some("new\n"));
    assert_eq!(stderr.as_deref(), Some(""));
    let token1 = updated_job.observation_token.unwrap();

    let (_, stdout, stderr, _, _, timed_out) = registry
        .job_log_for_auth(
            None,
            &job.job_id,
            None,
            None,
            Some(10),
            Some(&token1),
            Some(1),
        )
        .await
        .unwrap();
    assert_eq!(timed_out.wait_outcome, JobLogWaitOutcome::Timeout);
    assert!(!timed_out.changed);
    assert_eq!(
        timed_out.log_delta_status,
        crate::job_observation::JobLogDeltaStatus::Unchanged
    );
    assert_eq!(stdout.as_deref(), Some(""));
    assert_eq!(stderr.as_deref(), Some(""));
}
