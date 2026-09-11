use super::job_updates::{ShellJobStartMetadata, StructuredJobExecution};
use super::reconciliation::{
    reconcile_inventory_locked, recovery_timeout_sweep, validate_job_inventory,
    validate_job_inventory_without_project_membership, RECOVERY_SWEEP_PASS_CAP,
};
use super::state::{
    JobLifecycleState, JobRecoveryPhase, JobRecoveryReason, PendingShellRequest,
    ShellJobVisibility,
};
use super::{
    clamp_grace, job_recovery_grace_secs, now_ts, RunnerRegistry, RUNNER_ONLINE_WINDOW_SECS,
    JOB_RECOVERY_GRACE_SECS, LIVE_JOB_STREAM_RETENTION_BYTES,
};
use webcodex_core::runner_operation::{
    RunnerInvocationMetadata, RunnerJobOperation, RunnerOperation,
};
use crate::runner_protocol::{
    PersistentShellResult, RunnerJobUpdateRequest, RunnerPollRequest,
    RunnerProjectSummary, RunnerRequest, RunnerCapabilities,
    RunnerRegisterRequest, ShellCommandExecutionState, ShellJobContext, ShellJobInventory,
    ShellJobLogSnapshot, ShellJobOpRequest, ShellJobSnapshot, ShellJobStreamSnapshot,
    ShellJobValidationMetadata, ShellJobValidationProgress, ShellJobValidationStep,
    ShellProcessArgv, ShellScriptLanguage, ShellScriptPayload, JOB_INVENTORY_MAX_TERMINAL_JOBS,
    JOB_SNAPSHOT_STREAM_MAX_BYTES, JOB_TERMINAL_RETENTION_SECS,
};

const CLIENT_ID: &str = "oe";
const INSTANCE_A: &str = "instance-reconcile-a";
const INSTANCE_B: &str = "instance-reconcile-b";
const PROJECT_ID: &str = "demo";
const RUNTIME_PROJECT_ID: &str = "agent:oe:demo";
const SESSION_ID: &str = "wc_sess_job_reconciliation";

fn reconciliation_capabilities() -> RunnerCapabilities {
    RunnerCapabilities {
        jobs: true,
        async_jobs: true,
        async_shell_jobs: true,
        structured_process_argv: true,
        structured_script_payload: true,
        structured_execution_jobs: true,
        structured_validation_argv: true,
        structured_cargo_test_count_assertion: true,
        structured_cargo_test_execution_policy: true,
        job_state_reconciliation: true,
        coding_agent_runs: false,
        ..Default::default()
    }
}

fn project_summary() -> RunnerProjectSummary {
    RunnerProjectSummary {
        id: PROJECT_ID.to_string(),
        name: Some("Demo".to_string()),
        path: "/srv/demo".to_string(),
        allow_patch: true,
        kind: Some("rust".to_string()),
        registration_source: None,
        description: None,
        hooks: Vec::new(),
        disabled: false,
        revision: None,
        git_branch: Some("main".to_string()),
        git_head: None,
        git_dirty: None,
        updated_at: now_ts(),
        shell_profile: None,
    }
}

fn empty_inventory() -> ShellJobInventory {
    ShellJobInventory {
        active_complete: true,
        jobs: Vec::new(),
    }
}

fn register_request(instance: &str, inventory: ShellJobInventory) -> RunnerRegisterRequest {
    crate::test_support::current_runner_registration(RunnerRegisterRequest {
        client_id: CLIENT_ID.to_string(),
        runner_instance_id: instance.to_string(),
        runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
        display_name: Some("reconciliation test runner".to_string()),
        owner: Some("tester".to_string()),
        hostname: None,
        host_context: None,
        capabilities: reconciliation_capabilities(),
        policy: None,
        process_started_at: Some(1_700_000_000),
        build: None,
        job_concurrency_limit: None,
        job_inventory: Some(inventory),
        coding_agent_providers: None,
        coding_agent_inventory: None,
    })
}

async fn register(registry: &RunnerRegistry, instance: &str, inventory: ShellJobInventory) {
    registry
        .register(register_request(instance, inventory))
        .await
        .unwrap();
}

fn start_request(command: &str) -> ShellJobOpRequest {
    ShellJobOpRequest {
        op: "start".to_string(),
        client_id: Some(CLIENT_ID.to_string()),
        cwd: Some("/srv/demo".to_string()),
        command: Some(command.to_string()),
        timeout_secs: Some(120),
        job_id: None,
        since_stdout_line: None,
        since_stderr_line: None,
        tail_lines: None,
        limit: None,
        codex: None,
    }
}

fn cargo_validation_start_metadata(
    require_tests: Option<bool>,
    no_run: Option<bool>,
    minimum_tests: Option<u64>,
) -> ShellJobStartMetadata {
    let step = ShellJobValidationStep {
        name: "test".to_string(),
        program: "cargo".to_string(),
        args: vec!["test".to_string(), "focused".to_string()],
        env: Vec::new(),
    };
    ShellJobStartMetadata {
        project_id: Some(RUNTIME_PROJECT_ID.to_string()),
        session_id: Some(SESSION_ID.to_string()),
        project_cwd: Some("/srv/demo".to_string()),
        purpose: Some("test".to_string()),
        shell: Some("direct_argv".to_string()),
        validation_steps: vec![step.clone()],
        validation: Some(ShellJobValidationMetadata {
            tool: "cargo_test".to_string(),
            kind: "test".to_string(),
            steps: vec![step],
            effective_timeout_secs: 1800,
            sync_wait_secs: 30,
            adapter: "cargo_test".to_string(),
            validation_target_id: Some("target:0123456789abcdef01234567".to_string()),
            minimum_tests,
            require_tests,
            no_run,
        }),
        visibility: ShellJobVisibility::Public,
        ..Default::default()
    }
}

async fn start_and_take_over(
    registry: &RunnerRegistry,
    instance: &str,
) -> (
    crate::runner_protocol::ShellJobInfo,
    crate::runner_protocol::RunnerRequest,
) {
    let job = registry
        .start_job_with_metadata(
            start_request("printf 'one\\ntwo\\n'; sleep 30"),
            "tester".to_string(),
            ShellJobStartMetadata {
                project_id: Some(RUNTIME_PROJECT_ID.to_string()),
                session_id: Some(SESSION_ID.to_string()),
                project_cwd: Some("/srv/demo".to_string()),
                purpose: Some("test".to_string()),
                shell: Some("bash".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: CLIENT_ID.to_string(),
            runner_instance_id: instance.to_string(),
        })
        .await
        .unwrap()
        .expect("start request");
    assert_eq!(request.kind, "start_job");
    assert_eq!(request.job_id.as_deref(), Some(job.job_id.as_str()));
    let context = request.job_context.as_ref().expect("safe recovery context");
    assert_eq!(
        context.runtime_project_id.as_deref(),
        Some(RUNTIME_PROJECT_ID)
    );
    assert_eq!(context.workflow_session_id.as_deref(), Some(SESSION_ID));
    assert_eq!(context.command_preview, "printf 'one\\ntwo\\n'; sleep 30");
    assert!(!context.command_preview.contains("Authorization"));
    (job, request)
}

fn stream(tail: &str, first_retained_line: usize, truncated: bool) -> ShellJobStreamSnapshot {
    ShellJobStreamSnapshot {
        tail: tail.to_string(),
        first_retained_line,
        next_line: first_retained_line.saturating_add(tail.lines().count()),
        truncated,
    }
}

fn snapshot_from_request(
    job: &crate::runner_protocol::ShellJobInfo,
    request: &crate::runner_protocol::RunnerRequest,
    status: &str,
    update_seq: u64,
    stdout: ShellJobStreamSnapshot,
) -> ShellJobSnapshot {
    let terminal = matches!(
        status,
        "completed" | "failed" | "stopped" | "timeout" | "timed_out" | "cancelled" | "lost"
    );
    ShellJobSnapshot {
        job_id: job.job_id.clone(),
        request_id: request.request_id.clone(),
        status: status.to_string(),
        update_seq,
        created_at: job.created_at,
        started_at: Some(job.created_at + 1),
        ended_at: terminal.then_some(job.created_at + 2),
        exit_code: terminal.then_some(0),
        duration_ms: terminal.then_some(2_000),
        error: None,
        command_execution_state: None,
        context: request.job_context.clone().expect("job context"),
        stdout,
        stderr: ShellJobStreamSnapshot::default(),
        validation_progress: None,
        activity: None,
    }
}

fn update(
    instance: &str,
    job_id: &str,
    sequence: u64,
    status: &str,
    stdout_chunk: Option<&str>,
    finished: bool,
) -> RunnerJobUpdateRequest {
    RunnerJobUpdateRequest {
        client_id: CLIENT_ID.to_string(),
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

#[tokio::test]
async fn terminal_protocol_violation_during_recovery_keeps_execution_terminal_authoritative() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&registry, INSTANCE_A).await;
    registry
        .update_job(update(INSTANCE_A, &job.job_id, 1, "running", None, false))
        .await
        .unwrap();
    registry.reconcile_disconnect(CLIENT_ID, INSTANCE_A).await;

    {
        let inner = registry.inner.lock().await;
        let record = inner.jobs_by_id.get(&job.job_id).unwrap();
        assert_eq!(record.lifecycle, JobLifecycleState::Running);
        assert_eq!(record.recovery.phase, Some(JobRecoveryPhase::Recovering));
        assert!(record.recovery_active());
    }

    // A same-instance sequenced update while recovering must carry an
    // authoritative log snapshot. Deliberately attach validation progress to a
    // non-validation Job so executor protocol validation terminalizes it.
    let mut invalid = update(INSTANCE_A, &job.job_id, 2, "running", None, false);
    invalid.log_snapshot = Some(ShellJobLogSnapshot {
        stdout: ShellJobStreamSnapshot::default(),
        stderr: ShellJobStreamSnapshot::default(),
    });
    invalid.validation_progress = Some(ShellJobValidationProgress {
        completed: 0,
        current_step: None,
        failed_step: None,
    });
    let failed = registry.update_job(invalid).await.unwrap();
    assert_eq!(failed.status, "failed");
    assert_eq!(failed.recovery_state.as_deref(), Some("recovering"));
    assert!(failed
        .error
        .as_deref()
        .is_some_and(|error| error.contains("executor protocol violation")));

    {
        let mut inner = registry.inner.lock().await;
        let record = inner.jobs_by_id.get_mut(&job.job_id).unwrap();
        assert_eq!(record.lifecycle, JobLifecycleState::Failed);
        assert_eq!(record.recovery.phase, Some(JobRecoveryPhase::Recovering));
        assert!(
            !record.recovery_active(),
            "terminal execution lifecycle must fence retained recovery metadata"
        );
        record.recovery.recovering_since = Some(now_ts() - job_recovery_grace_secs() - 1);
    }

    // Terminal Job operations must retain the pre-refactor behavior even though
    // the compatibility recovery metadata is still projected.
    let stopped = registry
        .stop_job(&job.job_id, "tester".to_string())
        .await
        .unwrap();
    assert_eq!(stopped.status, "failed");
    let duplicate = registry
        .update_job(update(INSTANCE_A, &job.job_id, 3, "running", None, false))
        .await
        .unwrap();
    assert_eq!(duplicate.status, "failed");

    recovery_timeout_sweep(&registry).await;
    let after_sweep = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(after_sweep.status, "failed");
    assert_eq!(after_sweep.recovery_state.as_deref(), Some("recovering"));
}

#[tokio::test]
async fn validation_progress_accepts_coalesced_sequence_gaps_without_skipping_steps() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let steps = vec![
        ShellJobValidationStep {
            name: "format".to_string(),
            program: "cargo".to_string(),
            args: vec!["fmt".to_string(), "--".to_string(), "--check".to_string()],
            env: Vec::new(),
        },
        ShellJobValidationStep {
            name: "check".to_string(),
            program: "cargo".to_string(),
            args: vec!["check".to_string(), "--all-targets".to_string()],
            env: Vec::new(),
        },
        ShellJobValidationStep {
            name: "test".to_string(),
            program: "cargo".to_string(),
            args: vec!["test".to_string()],
            env: Vec::new(),
        },
    ];
    let job = registry
        .start_job_with_metadata(
            start_request("validation"),
            "tester".to_string(),
            ShellJobStartMetadata {
                project_id: Some(RUNTIME_PROJECT_ID.to_string()),
                session_id: Some(SESSION_ID.to_string()),
                project_cwd: Some("/srv/demo".to_string()),
                purpose: Some("validation".to_string()),
                shell: Some("bash".to_string()),
                validation_steps: steps,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: CLIENT_ID.to_string(),
            runner_instance_id: INSTANCE_A.to_string(),
        })
        .await
        .unwrap()
        .expect("validation start request");
    assert_eq!(request.kind, "start_validation_job");

    let validation_update = |sequence: u64,
                             status: &str,
                             completed: usize,
                             current_step: Option<&str>,
                             finished: bool| {
        let activity = current_step.map(|step| crate::runner_protocol::ShellJobActivity {
            state: crate::runner_protocol::ShellJobActivityState::Working,
            phase: match step {
                "format" => crate::runner_protocol::ShellJobActivityPhase::ValidationFormat,
                "check" => crate::runner_protocol::ShellJobActivityPhase::ValidationCheck,
                "test" => crate::runner_protocol::ShellJobActivityPhase::ValidationTest,
                other => panic!("unexpected validation step: {other}"),
            },
            source: crate::runner_protocol::ShellJobActivitySource::ValidationPlan,
        });
        RunnerJobUpdateRequest {
            client_id: CLIENT_ID.to_string(),
            runner_instance_id: INSTANCE_A.to_string(),
            job_id: job.job_id.clone(),
            request_id: Some(request.request_id.clone()),
            update_seq: Some(sequence),
            status: status.to_string(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: None,
            stderr_tail: None,
            log_snapshot: None,
            exit_code: finished.then_some(0),
            duration_ms: finished.then_some(2_000),
            error: None,
            command_execution_state: None,
            validation_progress: Some(ShellJobValidationProgress {
                completed,
                current_step: current_step.map(str::to_string),
                failed_step: None,
            }),
            activity,
            finished,
        }
    };

    for update in [
        validation_update(2, "running", 0, Some("format"), false),
        validation_update(37, "running", 1, Some("check"), false),
        validation_update(81, "running", 2, Some("test"), false),
    ] {
        registry.update_job(update).await.unwrap();
    }
    let before_stale = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(before_stale.last_update_seq, Some(81));
    assert_eq!(
        before_stale.activity,
        Some(crate::runner_protocol::ShellJobActivity {
            state: crate::runner_protocol::ShellJobActivityState::Working,
            phase: crate::runner_protocol::ShellJobActivityPhase::ValidationTest,
            source: crate::runner_protocol::ShellJobActivitySource::ValidationPlan,
        })
    );

    registry
        .update_job(validation_update(37, "running", 1, Some("check"), false))
        .await
        .unwrap();
    let after_stale = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(after_stale.last_update_seq, Some(81));
    assert_eq!(after_stale.activity, before_stale.activity);

    registry
        .update_job(validation_update(144, "completed", 3, None, true))
        .await
        .unwrap();

    let completed = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(completed.status, "completed");
    assert_eq!(completed.exit_code, Some(0));
    assert_eq!(completed.last_update_seq, Some(144));
    assert_eq!(completed.activity, None);
    assert_eq!(
        completed.validation_progress,
        Some(ShellJobValidationProgress {
            completed: 3,
            current_step: None,
            failed_step: None,
        })
    );
}

#[tokio::test]
async fn old_count_capable_runner_accepts_cargo_validation_without_explicit_execution_policy() {
    let registry = RunnerRegistry::default();
    let mut registration = register_request(INSTANCE_A, empty_inventory());
    registration
        .capabilities
        .structured_cargo_test_execution_policy = false;
    assert!(registration
        .capabilities
        .structured_cargo_test_count_assertion);
    registry.register(registration).await.unwrap();

    registry
        .start_job_with_metadata(
            start_request("validation"),
            "tester".to_string(),
            cargo_validation_start_metadata(None, None, None),
        )
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: CLIENT_ID.to_string(),
            runner_instance_id: INSTANCE_A.to_string(),
        })
        .await
        .unwrap()
        .expect("compatible Cargo validation request");
    assert_eq!(request.kind, "start_validation_job");
}

#[tokio::test]
async fn old_count_capable_runner_fails_closed_on_explicit_cargo_execution_policy() {
    for (label, require_tests, no_run, minimum_tests) in [
        ("require-false", Some(false), None, None),
        ("require-true", Some(true), None, Some(1)),
        ("no-run", None, Some(true), None),
    ] {
        let registry = RunnerRegistry::default();
        let mut registration = register_request(INSTANCE_A, empty_inventory());
        registration
            .capabilities
            .structured_cargo_test_execution_policy = false;
        assert!(registration
            .capabilities
            .structured_cargo_test_count_assertion);
        registry.register(registration).await.unwrap();

        let error = registry
            .start_job_with_metadata(
                start_request("validation"),
                "tester".to_string(),
                cargo_validation_start_metadata(require_tests, no_run, minimum_tests),
            )
            .await
            .unwrap_err();
        assert!(
            error.contains("structured_cargo_test_execution_policy_unavailable"),
            "case={label} error={error}"
        );
        assert!(registry.list_jobs(Some(100)).await.is_empty(), "case={label}");
    }
}

#[tokio::test]
async fn cargo_execution_policy_survives_inventory_roundtrip_and_server_restart() {
    let registry_a = RunnerRegistry::default();
    register(&registry_a, INSTANCE_A, empty_inventory()).await;
    let job = registry_a
        .start_job_with_metadata(
            start_request("validation"),
            "tester".to_string(),
            cargo_validation_start_metadata(Some(false), None, None),
        )
        .await
        .unwrap();
    let request = registry_a
        .poll(RunnerPollRequest {
            client_id: CLIENT_ID.to_string(),
            runner_instance_id: INSTANCE_A.to_string(),
        })
        .await
        .unwrap()
        .expect("validation Job request");
    let durable = request
        .job_context
        .as_ref()
        .and_then(|context| context.validation.as_ref())
        .expect("durable validation metadata");
    assert_eq!(durable.require_tests, Some(false));
    assert_eq!(durable.no_run, None);

    let mut snapshot = snapshot_from_request(&job, &request, "running", 2, stream("", 1, false));
    snapshot.validation_progress = Some(ShellJobValidationProgress {
        completed: 0,
        current_step: Some("test".to_string()),
        failed_step: None,
    });
    let inventory: ShellJobInventory = serde_json::from_value(
        serde_json::to_value(ShellJobInventory {
            active_complete: true,
            jobs: vec![snapshot],
        })
        .unwrap(),
    )
    .unwrap();
    let registry_b = RunnerRegistry::default();
    register(&registry_b, INSTANCE_A, inventory).await;
    let restored = registry_b.get_job(&job.job_id).await.unwrap();
    let restored_validation = restored.validation.expect("restored validation metadata");
    assert_eq!(restored_validation.require_tests, Some(false));
    assert_eq!(restored_validation.no_run, None);
    assert!(restored.recovered_after_server_restart);
}

#[tokio::test]
async fn cargo_test_count_assertion_survives_inventory_roundtrip_and_server_restart() {
    let registry_a = RunnerRegistry::default();
    register(&registry_a, INSTANCE_A, empty_inventory()).await;
    let step = ShellJobValidationStep {
        name: "test".to_string(),
        program: "cargo".to_string(),
        args: vec!["test".to_string(), "focused".to_string()],
        env: Vec::new(),
    };
    let target = "target:0123456789abcdef01234567";
    let job = registry_a
        .start_job_with_metadata(
            start_request("validation"),
            "tester".to_string(),
            ShellJobStartMetadata {
                project_id: Some(RUNTIME_PROJECT_ID.to_string()),
                session_id: Some(SESSION_ID.to_string()),
                project_cwd: Some("/srv/demo".to_string()),
                purpose: Some("test".to_string()),
                shell: Some("direct_argv".to_string()),
                validation_steps: vec![step.clone()],
                validation: Some(ShellJobValidationMetadata {
                    tool: "cargo_test".to_string(),
                    kind: "test".to_string(),
                    steps: vec![step],
                    effective_timeout_secs: 1800,
                    sync_wait_secs: 30,
                    adapter: "cargo_test".to_string(),
                    validation_target_id: Some(target.to_string()),
                    minimum_tests: Some(6),
                    require_tests: None,
                    no_run: None,
                }),
                visibility: ShellJobVisibility::Public,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let request = registry_a
        .poll(RunnerPollRequest {
            client_id: CLIENT_ID.to_string(),
            runner_instance_id: INSTANCE_A.to_string(),
        })
        .await
        .unwrap()
        .expect("validation Job request");
    assert_eq!(request.kind, "start_validation_job");
    assert_eq!(
        request
            .job_context
            .as_ref()
            .and_then(|context| context.validation.as_ref())
            .and_then(|validation| validation.minimum_tests),
        Some(6)
    );

    let mut snapshot = snapshot_from_request(
        &job,
        &request,
        "running",
        2,
        stream("running 4 tests\n", 1, false),
    );
    snapshot.validation_progress = Some(ShellJobValidationProgress {
        completed: 0,
        current_step: Some("test".to_string()),
        failed_step: None,
    });
    snapshot.activity = Some(crate::runner_protocol::ShellJobActivity {
        state: crate::runner_protocol::ShellJobActivityState::Working,
        phase: crate::runner_protocol::ShellJobActivityPhase::ValidationTest,
        source: crate::runner_protocol::ShellJobActivitySource::ValidationPlan,
    });
    let expected_activity = snapshot.activity;
    let inventory: ShellJobInventory = serde_json::from_value(
        serde_json::to_value(ShellJobInventory {
            active_complete: true,
            jobs: vec![snapshot],
        })
        .unwrap(),
    )
    .unwrap();
    let registry_b = RunnerRegistry::default();
    register(&registry_b, INSTANCE_A, inventory).await;
    let restored = registry_b.get_job(&job.job_id).await.unwrap();
    let restored_validation = restored.validation.expect("restored validation metadata");
    assert_eq!(restored_validation.minimum_tests, Some(6));
    assert_eq!(
        restored_validation.validation_target_id.as_deref(),
        Some(target)
    );
    assert_eq!(restored.status, "running");
    assert_eq!(restored.activity, expected_activity);
    assert!(restored.recovered_after_server_restart);
}

#[tokio::test]
async fn reconciliation_rejects_cross_product_first_class_go_test_metadata() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, request) = start_and_take_over(&registry, INSTANCE_A).await;
    let mut snapshot = snapshot_from_request(&job, &request, "running", 2, stream("", 1, false));
    let cargo_step = ShellJobValidationStep {
        name: "test".to_string(),
        program: "cargo".to_string(),
        args: vec!["test".to_string(), "tool_runtime".to_string()],
        env: Vec::new(),
    };
    snapshot.context.purpose = Some("validation".to_string());
    snapshot.context.validation_steps = vec!["test".to_string()];
    snapshot.context.validation = Some(ShellJobValidationMetadata {
        tool: "go_test".to_string(),
        kind: "test".to_string(),
        steps: vec![cargo_step],
        effective_timeout_secs: 1800,
        sync_wait_secs: 10,
        adapter: "go_test".to_string(),
        validation_target_id: None,
        minimum_tests: None,
        require_tests: None,
        no_run: None,
    });
    let inventory = ShellJobInventory {
        active_complete: true,
        jobs: vec![snapshot],
    };

    let error = validate_job_inventory(CLIENT_ID, &[project_summary()], &inventory).unwrap_err();
    assert!(error.contains("validation metadata is invalid"), "{error}");
}

#[tokio::test]
async fn job_reconciliation_server_restart_restores_running_job_and_completion() {
    let registry_a = RunnerRegistry::default();
    register(&registry_a, INSTANCE_A, empty_inventory()).await;
    let (job, request) = start_and_take_over(&registry_a, INSTANCE_A).await;
    let snapshot = snapshot_from_request(&job, &request, "running", 2, stream("one\n", 1, false));

    let registry_b = RunnerRegistry::default();
    register(
        &registry_b,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![snapshot],
        },
    )
    .await;

    let restored = registry_b.get_job(&job.job_id).await.unwrap();
    assert_eq!(restored.job_id, job.job_id);
    assert_eq!(restored.status, "running");
    assert_eq!(restored.project_id.as_deref(), Some(RUNTIME_PROJECT_ID));
    assert_eq!(restored.session_id.as_deref(), Some(SESSION_ID));
    assert_eq!(restored.project_cwd.as_deref(), Some("/srv/demo"));
    assert_eq!(restored.cwd.as_deref(), Some("/srv/demo"));
    assert_eq!(restored.purpose.as_deref(), Some("test"));
    assert_eq!(restored.shell.as_deref(), Some("bash"));
    assert!(restored.recovered_after_server_restart);
    assert_eq!(restored.last_update_seq, Some(2));
    assert_eq!(registry_b.list_jobs(Some(10)).await.len(), 1);

    registry_b
        .update_job(update(
            INSTANCE_A,
            &job.job_id,
            3,
            "completed",
            Some("two\n"),
            true,
        ))
        .await
        .unwrap();
    let completed = registry_b.get_job(&job.job_id).await.unwrap();
    assert_eq!(completed.status, "completed");
    assert_eq!(completed.exit_code, Some(0));
    assert_eq!(completed.last_update_seq, Some(3));
    let (_, stdout, _, next_stdout, _) = registry_b
        .job_log(&job.job_id, Some(1), None, None)
        .await
        .unwrap();
    assert_eq!(stdout.as_deref(), Some("one\ntwo\n"));
    assert_eq!(next_stdout, 3);
}

#[tokio::test]
async fn structured_process_reconciliation_restores_active_and_terminal_evidence_without_redispatch(
) {
    let registry_a = RunnerRegistry::default();
    register(&registry_a, INSTANCE_A, empty_inventory()).await;
    let job = registry_a
        .start_job_with_metadata(
            start_request(""),
            "tester".to_string(),
            ShellJobStartMetadata {
                project_id: Some(RUNTIME_PROJECT_ID.to_string()),
                session_id: Some(SESSION_ID.to_string()),
                project_cwd: Some("/srv/demo".to_string()),
                purpose: Some("test".to_string()),
                shell: Some("direct_argv".to_string()),
                visibility: ShellJobVisibility::HiddenUntilHandoff,
                structured_execution: Some(StructuredJobExecution::Process(ShellProcessArgv {
                    executable: "/bin/echo".to_string(),
                    args: vec!["literal;$(touch never-executed)".to_string()],
                })),
                validation_identity: Some("assertion:0123456789abcdef01234567".to_string()),
                validation_tool: Some("cargo_test".to_string()),
                assertion_name: Some("restart process assertion".to_string()),
                stdin: Some("typed stdin".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let request = registry_a
        .poll(RunnerPollRequest {
            client_id: CLIENT_ID.to_string(),
            runner_instance_id: INSTANCE_A.to_string(),
        })
        .await
        .unwrap()
        .expect("typed process Job request");
    assert_eq!(request.kind, "start_process_job");
    assert_eq!(request.command, "");
    assert!(request.process.is_some());
    assert!(request.script.is_none());

    let running_snapshot =
        snapshot_from_request(&job, &request, "running", 2, stream("started\n", 1, false));
    let registry_b = RunnerRegistry::default();
    register(
        &registry_b,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![running_snapshot],
        },
    )
    .await;
    let restored = registry_b.get_job(&job.job_id).await.unwrap();
    assert_eq!(restored.job_id, job.job_id);
    assert_eq!(restored.status, "running");
    assert_eq!(restored.kind, "run_process");
    assert_eq!(
        restored
            .structured_execution
            .as_ref()
            .map(|metadata| metadata.execution_source.as_str()),
        Some("run_process")
    );
    assert!(restored.recovered_after_server_restart);
    let restored_metadata = restored.structured_execution.as_ref().unwrap();
    assert_eq!(
        restored_metadata.validation_identity.as_deref(),
        Some("assertion:0123456789abcdef01234567")
    );
    assert_eq!(
        restored_metadata.validation_tool.as_deref(),
        Some("cargo_test")
    );
    assert_eq!(
        restored_metadata.assertion_name.as_deref(),
        Some("restart process assertion")
    );
    assert!(
        registry_b
            .poll(RunnerPollRequest {
                client_id: CLIENT_ID.to_string(),
                runner_instance_id: INSTANCE_A.to_string(),
            })
            .await
            .unwrap()
            .is_none(),
        "active snapshot reconciliation must not enqueue typed process input"
    );

    let mut terminal_snapshot = snapshot_from_request(
        &job,
        &request,
        "failed",
        3,
        stream("retained process stdout\n", 42, true),
    );
    terminal_snapshot.exit_code = Some(7);
    terminal_snapshot.stderr = stream("retained process stderr\n", 9, true);
    terminal_snapshot.command_execution_state = Some(ShellCommandExecutionState::Completed);
    register(
        &registry_b,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![terminal_snapshot.clone()],
        },
    )
    .await;
    let completed = registry_b.get_job(&job.job_id).await.unwrap();
    assert_eq!(completed.status, "failed");
    assert_eq!(completed.exit_code, Some(7));
    assert_eq!(
        completed.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    assert_eq!(completed.stdout_retained_from_line, Some(42));
    assert_eq!(completed.stderr_retained_from_line, Some(9));
    assert!(completed.stdout_log_truncated);
    assert!(completed.stderr_log_truncated);
    let (_, stdout, stderr, _, _) = registry_b
        .job_log(&job.job_id, None, None, None)
        .await
        .unwrap();
    assert_eq!(stdout.as_deref(), Some("retained process stdout\n"));
    assert_eq!(stderr.as_deref(), Some("retained process stderr\n"));
    assert!(
        registry_b
            .poll(RunnerPollRequest {
                client_id: CLIENT_ID.to_string(),
                runner_instance_id: INSTANCE_A.to_string(),
            })
            .await
            .unwrap()
            .is_none(),
        "terminal snapshot reconciliation must not enqueue a replacement process"
    );

    let registry_c = RunnerRegistry::default();
    register(
        &registry_c,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![terminal_snapshot],
        },
    )
    .await;
    let recovered = registry_c.get_job(&job.job_id).await.unwrap();
    assert_eq!(recovered.job_id, job.job_id);
    assert_eq!(recovered.status, "failed");
    assert_eq!(recovered.exit_code, Some(7));
    assert_eq!(
        recovered.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    assert_eq!(recovered.project_id.as_deref(), Some(RUNTIME_PROJECT_ID));
    assert_eq!(recovered.session_id.as_deref(), Some(SESSION_ID));
    assert_eq!(recovered.project_cwd.as_deref(), Some("/srv/demo"));
    assert_eq!(recovered.cwd.as_deref(), Some("/srv/demo"));
    assert_eq!(recovered.purpose.as_deref(), Some("test"));
    assert_eq!(recovered.shell.as_deref(), Some("direct_argv"));
    assert_eq!(recovered.stdout_retained_from_line, Some(42));
    assert_eq!(recovered.stderr_retained_from_line, Some(9));
    assert!(recovered.stdout_log_truncated);
    assert!(recovered.stderr_log_truncated);
    let metadata = recovered
        .structured_execution
        .as_ref()
        .expect("safe process metadata");
    assert_eq!(metadata.execution_source, "run_process");
    assert_eq!(metadata.language, None);
    assert_eq!(metadata.script_bytes, None);
    assert_eq!(metadata.arg_count, 1);
    assert!(metadata.stdin_present);
    assert!(recovered.recovered_after_server_restart);
    assert_eq!(
        recovered.recovery_reason_code.as_deref(),
        Some("server_restart_reconciliation")
    );
    let (_, stdout, stderr, _, _) = registry_c
        .job_log(&job.job_id, None, None, None)
        .await
        .unwrap();
    assert_eq!(stdout.as_deref(), Some("retained process stdout\n"));
    assert_eq!(stderr.as_deref(), Some("retained process stderr\n"));
    assert_eq!(registry_c.list_jobs(Some(10)).await.len(), 1);
    assert!(
        registry_c
            .poll(RunnerPollRequest {
                client_id: CLIENT_ID.to_string(),
                runner_instance_id: INSTANCE_A.to_string(),
            })
            .await
            .unwrap()
            .is_none(),
        "fresh-registry terminal recovery must not enqueue typed process input"
    );
    assert!(
        !serde_json::to_string(&recovered)
            .unwrap()
            .contains("typed stdin"),
        "recovered durable state must not contain typed stdin"
    );
}

#[tokio::test]
async fn javascript_structured_job_start_requires_additive_runner_capability() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let metadata = || ShellJobStartMetadata {
        project_id: Some(RUNTIME_PROJECT_ID.to_string()),
        session_id: Some(SESSION_ID.to_string()),
        project_cwd: Some("/srv/demo".to_string()),
        purpose: Some("operation".to_string()),
        shell: Some("javascript".to_string()),
        visibility: ShellJobVisibility::HiddenUntilHandoff,
        structured_execution: Some(StructuredJobExecution::Script(ShellScriptPayload {
            language: ShellScriptLanguage::Javascript,
            script: "await Promise.resolve();\n".to_string(),
            args: Vec::new(),
        })),
        ..Default::default()
    };

    let error = registry
        .start_job_with_metadata(start_request(""), "tester".to_string(), metadata())
        .await
        .unwrap_err();
    assert!(error.contains("structured_script_javascript"), "{error}");

    let mut upgraded = register_request(INSTANCE_A, empty_inventory());
    upgraded.capabilities.structured_script_javascript = true;
    registry.register(upgraded).await.unwrap();
    let job = registry
        .start_job_with_metadata(start_request(""), "tester".to_string(), metadata())
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: CLIENT_ID.to_string(),
            runner_instance_id: INSTANCE_A.to_string(),
        })
        .await
        .unwrap()
        .expect("JavaScript script Job request");
    assert_eq!(request.job_id.as_deref(), Some(job.job_id.as_str()));
    assert_eq!(
        request.script.as_ref().map(|script| script.language),
        Some(ShellScriptLanguage::Javascript)
    );
}

#[tokio::test]
async fn typescript_structured_job_start_requires_additive_runner_capability() {
    let registry = RunnerRegistry::default();
    let mut old_style = register_request(INSTANCE_A, empty_inventory());
    old_style.capabilities.structured_script_javascript = true;
    old_style.capabilities.structured_script_typescript = false;
    registry.register(old_style).await.unwrap();
    let metadata = || ShellJobStartMetadata {
        project_id: Some(RUNTIME_PROJECT_ID.to_string()),
        session_id: Some(SESSION_ID.to_string()),
        project_cwd: Some("/srv/demo".to_string()),
        purpose: Some("operation".to_string()),
        shell: Some("typescript".to_string()),
        visibility: ShellJobVisibility::HiddenUntilHandoff,
        structured_execution: Some(StructuredJobExecution::Script(ShellScriptPayload {
            language: ShellScriptLanguage::Typescript,
            script: "const value: string = 'ok';\nvoid value;\n".to_string(),
            args: Vec::new(),
        })),
        ..Default::default()
    };

    let error = registry
        .start_job_with_metadata(start_request(""), "tester".to_string(), metadata())
        .await
        .unwrap_err();
    assert!(error.contains("structured_script_typescript"), "{error}");

    let mut upgraded = register_request(INSTANCE_A, empty_inventory());
    upgraded.capabilities.structured_script_javascript = true;
    upgraded.capabilities.structured_script_typescript = true;
    registry.register(upgraded).await.unwrap();
    let job = registry
        .start_job_with_metadata(start_request(""), "tester".to_string(), metadata())
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: CLIENT_ID.to_string(),
            runner_instance_id: INSTANCE_A.to_string(),
        })
        .await
        .unwrap()
        .expect("TypeScript script Job request");
    assert_eq!(request.job_id.as_deref(), Some(job.job_id.as_str()));
    assert_eq!(
        request.script.as_ref().map(|script| script.language),
        Some(ShellScriptLanguage::Typescript)
    );
}

#[tokio::test]
async fn terminal_structured_script_snapshot_is_recovered_with_safe_metadata_without_redispatch() {
    let registry_a = RunnerRegistry::default();
    register(&registry_a, INSTANCE_A, empty_inventory()).await;
    let script_body = "printf 'retained script output\\n'\n".to_string();
    let script_arg = "private structured script arg".to_string();
    let script_stdin = "private structured script stdin".to_string();
    let job = registry_a
        .start_job_with_metadata(
            start_request(""),
            "tester".to_string(),
            ShellJobStartMetadata {
                project_id: Some(RUNTIME_PROJECT_ID.to_string()),
                session_id: Some(SESSION_ID.to_string()),
                project_cwd: Some("/srv/demo".to_string()),
                purpose: Some("test".to_string()),
                shell: Some("bash".to_string()),
                visibility: ShellJobVisibility::HiddenUntilHandoff,
                structured_execution: Some(StructuredJobExecution::Script(ShellScriptPayload {
                    language: ShellScriptLanguage::Bash,
                    script: script_body.clone(),
                    args: vec![script_arg.clone()],
                })),
                validation_identity: Some("assertion:fedcba9876543210fedcba98".to_string()),
                validation_tool: None,
                assertion_name: Some("restart script assertion".to_string()),
                stdin: Some(script_stdin.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let request = registry_a
        .poll(RunnerPollRequest {
            client_id: CLIENT_ID.to_string(),
            runner_instance_id: INSTANCE_A.to_string(),
        })
        .await
        .unwrap()
        .expect("typed script Job request");
    assert_eq!(request.kind, "start_script_job");
    assert_eq!(request.command, "");
    assert!(request.process.is_none());
    assert!(request.script.is_some());

    let mut terminal_snapshot = snapshot_from_request(
        &job,
        &request,
        "completed",
        2,
        stream("retained script stdout\n", 17, true),
    );
    terminal_snapshot.stderr = stream("retained script stderr\n", 4, true);
    terminal_snapshot.command_execution_state = Some(ShellCommandExecutionState::Completed);

    let registry_b = RunnerRegistry::default();
    register(
        &registry_b,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![terminal_snapshot],
        },
    )
    .await;

    let recovered = registry_b.get_job(&job.job_id).await.unwrap();
    assert_eq!(recovered.job_id, job.job_id);
    assert_eq!(recovered.kind, "run_script");
    assert_eq!(recovered.status, "completed");
    assert_eq!(recovered.exit_code, Some(0));
    assert_eq!(
        recovered.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    assert_eq!(recovered.project_id.as_deref(), Some(RUNTIME_PROJECT_ID));
    assert_eq!(recovered.session_id.as_deref(), Some(SESSION_ID));
    assert_eq!(recovered.project_cwd.as_deref(), Some("/srv/demo"));
    assert_eq!(recovered.cwd.as_deref(), Some("/srv/demo"));
    assert_eq!(recovered.purpose.as_deref(), Some("test"));
    assert_eq!(recovered.shell.as_deref(), Some("bash"));
    assert_eq!(recovered.stdout_retained_from_line, Some(17));
    assert_eq!(recovered.stderr_retained_from_line, Some(4));
    assert!(recovered.stdout_log_truncated);
    assert!(recovered.stderr_log_truncated);
    let metadata = recovered
        .structured_execution
        .as_ref()
        .expect("safe script metadata");
    assert_eq!(metadata.execution_source, "run_script");
    assert_eq!(metadata.language, Some(ShellScriptLanguage::Bash));
    assert_eq!(metadata.script_bytes, Some(script_body.len()));
    assert_eq!(metadata.arg_count, 1);
    assert!(metadata.stdin_present);
    assert_eq!(
        metadata.validation_identity.as_deref(),
        Some("assertion:fedcba9876543210fedcba98")
    );
    assert!(metadata.validation_tool.is_none());
    assert_eq!(
        metadata.assertion_name.as_deref(),
        Some("restart script assertion")
    );
    assert!(recovered.recovered_after_server_restart);
    let (_, stdout, stderr, _, _) = registry_b
        .job_log(&job.job_id, None, None, None)
        .await
        .unwrap();
    assert_eq!(stdout.as_deref(), Some("retained script stdout\n"));
    assert_eq!(stderr.as_deref(), Some("retained script stderr\n"));
    let durable = serde_json::to_string(&recovered).unwrap();
    for raw in [&script_body, &script_arg, &script_stdin] {
        assert!(
            !durable.contains(raw),
            "recovered durable state leaked raw structured script input"
        );
    }
    assert_eq!(registry_b.list_jobs(Some(10)).await.len(), 1);
    assert!(
        registry_b
            .poll(RunnerPollRequest {
                client_id: CLIENT_ID.to_string(),
                runner_instance_id: INSTANCE_A.to_string(),
            })
            .await
            .unwrap()
            .is_none(),
        "fresh-registry terminal recovery must not enqueue typed script input"
    );
}

#[tokio::test]
async fn projected_hidden_structured_terminal_is_suppressed_only_by_same_server_history() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let job = registry
        .start_job_with_metadata(
            start_request(""),
            "tester".to_string(),
            ShellJobStartMetadata {
                project_id: Some(RUNTIME_PROJECT_ID.to_string()),
                session_id: Some(SESSION_ID.to_string()),
                project_cwd: Some("/srv/demo".to_string()),
                purpose: Some("test".to_string()),
                shell: Some("direct_argv".to_string()),
                visibility: ShellJobVisibility::HiddenUntilHandoff,
                structured_execution: Some(StructuredJobExecution::Process(ShellProcessArgv {
                    executable: "/bin/echo".to_string(),
                    args: vec!["safe retained argument".to_string()],
                })),
                stdin: Some("private projected stdin".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: CLIENT_ID.to_string(),
            runner_instance_id: INSTANCE_A.to_string(),
        })
        .await
        .unwrap()
        .expect("typed process Job request");
    assert_eq!(request.kind, "start_process_job");

    registry
        .update_job(update(
            INSTANCE_A,
            &job.job_id,
            1,
            "running",
            Some("started\n"),
            false,
        ))
        .await
        .unwrap();
    let mut terminal_update = update(
        INSTANCE_A,
        &job.job_id,
        2,
        "failed",
        Some("retained process stdout\n"),
        true,
    );
    terminal_update.stderr_chunk = Some("retained process stderr\n".to_string());
    terminal_update.exit_code = Some(7);
    terminal_update.command_execution_state = Some(ShellCommandExecutionState::Completed);
    registry.update_job(terminal_update).await.unwrap();

    let (projected, stdout, stderr, _, _) = registry
        .hidden_job_log_for_auth(None, &job.job_id, None)
        .await
        .unwrap();
    assert_eq!(projected.status, "failed");
    assert_eq!(projected.exit_code, Some(7));
    assert_eq!(
        projected.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    assert_eq!(
        stdout.as_deref(),
        Some("started\nretained process stdout\n")
    );
    assert_eq!(stderr.as_deref(), Some("retained process stderr\n"));

    let mut retained_snapshot = snapshot_from_request(
        &job,
        &request,
        "failed",
        2,
        stream("started\nretained process stdout\n", 42, true),
    );
    retained_snapshot.exit_code = Some(7);
    retained_snapshot.stderr = stream("retained process stderr\n", 9, true);
    retained_snapshot.command_execution_state = Some(ShellCommandExecutionState::Completed);

    assert!(
        registry
            .remove_projected_hidden_structured_job_record(&job.job_id)
            .await
    );
    assert!(registry.list_jobs(Some(10)).await.is_empty());
    assert!(registry.get_job(&job.job_id).await.is_err());

    register(
        &registry,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![retained_snapshot.clone()],
        },
    )
    .await;
    assert!(registry.get_job(&job.job_id).await.is_err());
    assert!(registry.list_jobs(Some(10)).await.is_empty());
    assert!(
        registry
            .poll(RunnerPollRequest {
                client_id: CLIENT_ID.to_string(),
                runner_instance_id: INSTANCE_A.to_string(),
            })
            .await
            .unwrap()
            .is_none(),
        "same-Server suppression must not enqueue typed process input"
    );
    {
        let inner = registry.inner.lock().await;
        assert!(inner.pending_by_id.is_empty());
        assert!(inner
            .queues_by_runner
            .get(CLIENT_ID)
            .is_none_or(|queue| queue.is_empty()));
    }

    let fresh_registry = RunnerRegistry::default();
    register(
        &fresh_registry,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![retained_snapshot],
        },
    )
    .await;
    let recovered = fresh_registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(recovered.job_id, job.job_id);
    assert_eq!(recovered.status, "failed");
    assert_eq!(recovered.exit_code, Some(7));
    assert_eq!(
        recovered.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    assert_eq!(recovered.project_id.as_deref(), Some(RUNTIME_PROJECT_ID));
    assert_eq!(recovered.session_id.as_deref(), Some(SESSION_ID));
    assert_eq!(recovered.project_cwd.as_deref(), Some("/srv/demo"));
    assert_eq!(recovered.cwd.as_deref(), Some("/srv/demo"));
    assert_eq!(recovered.purpose.as_deref(), Some("test"));
    assert_eq!(recovered.shell.as_deref(), Some("direct_argv"));
    assert_eq!(recovered.stdout_retained_from_line, Some(42));
    assert_eq!(recovered.stderr_retained_from_line, Some(9));
    assert!(recovered.stdout_log_truncated);
    assert!(recovered.stderr_log_truncated);
    assert!(recovered.recovered_after_server_restart);
    assert_eq!(
        recovered.recovery_reason_code.as_deref(),
        Some("server_restart_reconciliation")
    );
    let metadata = recovered
        .structured_execution
        .as_ref()
        .expect("safe structured process metadata");
    assert_eq!(metadata.execution_source, "run_process");
    assert_eq!(metadata.arg_count, 1);
    assert!(metadata.stdin_present);
    let (_, stdout, stderr, _, _) = fresh_registry
        .job_log(&job.job_id, None, None, None)
        .await
        .unwrap();
    assert_eq!(
        stdout.as_deref(),
        Some("started\nretained process stdout\n")
    );
    assert_eq!(stderr.as_deref(), Some("retained process stderr\n"));
    assert_eq!(fresh_registry.list_jobs(Some(10)).await.len(), 1);
    assert!(
        fresh_registry
            .poll(RunnerPollRequest {
                client_id: CLIENT_ID.to_string(),
                runner_instance_id: INSTANCE_A.to_string(),
            })
            .await
            .unwrap()
            .is_none(),
        "fresh-Server recovery must not enqueue typed process input"
    );
    assert!(!serde_json::to_string(&recovered)
        .unwrap()
        .contains("private projected stdin"));
}

#[tokio::test]
async fn projected_hidden_raw_shell_terminal_does_not_resurrect_on_same_instance_reconnect() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let job = registry
        .start_job_with_metadata(
            start_request("printf raw-shell"),
            "tester".to_string(),
            ShellJobStartMetadata {
                project_id: Some(RUNTIME_PROJECT_ID.to_string()),
                session_id: Some(SESSION_ID.to_string()),
                project_cwd: Some("/srv/demo".to_string()),
                purpose: Some("test".to_string()),
                shell: Some("bash".to_string()),
                visibility: ShellJobVisibility::HiddenUntilHandoff,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: CLIENT_ID.to_string(),
            runner_instance_id: INSTANCE_A.to_string(),
        })
        .await
        .unwrap()
        .expect("raw shell Job request");
    assert_eq!(request.kind, "start_job");

    registry
        .update_job(update(
            INSTANCE_A,
            &job.job_id,
            1,
            "running",
            Some("raw started\n"),
            false,
        ))
        .await
        .unwrap();
    let mut terminal_update = update(
        INSTANCE_A,
        &job.job_id,
        2,
        "completed",
        Some("raw done\n"),
        true,
    );
    terminal_update.command_execution_state = Some(ShellCommandExecutionState::Completed);
    registry.update_job(terminal_update).await.unwrap();

    let mut retained_snapshot = snapshot_from_request(
        &job,
        &request,
        "completed",
        2,
        stream("raw started\nraw done\n", 1, false),
    );
    retained_snapshot.command_execution_state = Some(ShellCommandExecutionState::Completed);

    assert!(
        registry
            .remove_projected_hidden_terminal_job_record(&job.job_id)
            .await
    );
    assert!(registry.list_jobs(Some(10)).await.is_empty());

    register(
        &registry,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![retained_snapshot.clone()],
        },
    )
    .await;

    assert!(
        registry.get_job(&job.job_id).await.is_err(),
        "same-Server inventory replay must not resurrect a terminal hidden raw shell Job"
    );
    assert!(registry.list_jobs(Some(10)).await.is_empty());

    let fresh_registry = RunnerRegistry::default();
    register(
        &fresh_registry,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![retained_snapshot],
        },
    )
    .await;
    let recovered = fresh_registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(recovered.status, "completed");
    assert!(recovered.recovered_after_server_restart);
}

#[tokio::test]
async fn projected_hidden_terminal_removes_after_runner_instance_replacement() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let job = registry
        .start_job_with_metadata(
            start_request("printf projected-before-replacement"),
            "tester".to_string(),
            ShellJobStartMetadata {
                project_id: Some(RUNTIME_PROJECT_ID.to_string()),
                session_id: Some(SESSION_ID.to_string()),
                project_cwd: Some("/srv/demo".to_string()),
                purpose: Some("test".to_string()),
                shell: Some("bash".to_string()),
                visibility: ShellJobVisibility::HiddenUntilHandoff,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: CLIENT_ID.to_string(),
            runner_instance_id: INSTANCE_A.to_string(),
        })
        .await
        .unwrap()
        .expect("hidden Job request");
    registry
        .update_job(update(
            INSTANCE_A,
            &job.job_id,
            1,
            "completed",
            Some("projected result\n"),
            true,
        ))
        .await
        .unwrap();

    registry
        .set_last_seen_for_test(
            CLIENT_ID,
            now_ts().saturating_sub(RUNNER_ONLINE_WINDOW_SECS + 1),
        )
        .await;
    register(&registry, INSTANCE_B, empty_inventory()).await;

    assert!(
        registry
            .remove_projected_hidden_terminal_job_record(&job.job_id)
            .await,
        "a terminal result already projected to the caller must not remain hidden forever when the Runner lease changes"
    );
    let inner = registry.inner.lock().await;
    assert!(!inner.jobs_by_id.contains_key(&job.job_id));
    assert!(!inner.request_to_job.contains_key(&request.request_id));
    assert!(inner
        .runners
        .get(CLIENT_ID)
        .unwrap()
        .projected_structured_terminal_suppressions
        .is_empty());
}

#[tokio::test]
async fn projected_structured_terminal_suppressions_are_bounded_and_expire() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let now = now_ts();
    {
        let mut inner = registry.inner.lock().await;
        let client = inner.runners.get_mut(CLIENT_ID).unwrap();
        for index in 0..=JOB_INVENTORY_MAX_TERMINAL_JOBS {
            client.remember_projected_structured_terminal(
                format!("projected-job-{index}"),
                format!("projected-request-{index}"),
                now,
            );
        }
        assert_eq!(
            client.projected_structured_terminal_suppressions.len(),
            JOB_INVENTORY_MAX_TERMINAL_JOBS
        );
        assert_eq!(
            client
                .projected_structured_terminal_suppressions
                .front()
                .map(|suppression| suppression.job_id.as_str()),
            Some("projected-job-1")
        );
        assert!(client.suppresses_projected_structured_terminal(
            CLIENT_ID,
            INSTANCE_A,
            "projected-job-64",
            "projected-request-64",
            now,
        ));
        assert!(!client.suppresses_projected_structured_terminal(
            CLIENT_ID,
            INSTANCE_A,
            "projected-job-64",
            "wrong-request",
            now,
        ));
        for suppression in &mut client.projected_structured_terminal_suppressions {
            suppression.expires_at = now;
        }
    }

    recovery_timeout_sweep(&registry).await;

    let inner = registry.inner.lock().await;
    assert!(inner.runners[CLIENT_ID]
        .projected_structured_terminal_suppressions
        .is_empty());
}

#[tokio::test]
async fn terminal_observed_inventory_replay_is_idempotent() {
    let registry_a = RunnerRegistry::default();
    register(&registry_a, INSTANCE_A, empty_inventory()).await;
    let (job, request) = start_and_take_over(&registry_a, INSTANCE_A).await;
    let mut snapshot = snapshot_from_request(
        &job,
        &request,
        "completed",
        7,
        stream("offline output\n", 4, true),
    );
    snapshot.context.command_preview = "validation: check".to_string();
    snapshot.context.validation_steps = vec!["check".to_string()];
    snapshot.validation_progress = Some(ShellJobValidationProgress {
        completed: 1,
        current_step: None,
        failed_step: None,
    });
    let inventory = ShellJobInventory {
        active_complete: true,
        jobs: vec![snapshot],
    };

    let registry_b = RunnerRegistry::default();
    register(&registry_b, INSTANCE_A, inventory.clone()).await;
    let first = registry_b.get_job(&job.job_id).await.unwrap();
    assert_eq!(first.status, "completed");
    assert_eq!(first.exit_code, Some(0));
    assert_eq!(first.duration_ms, Some(2_000));
    assert_eq!(
        first.validation_progress,
        Some(ShellJobValidationProgress {
            completed: 1,
            current_step: None,
            failed_step: None,
        })
    );
    assert_eq!(first.stdout_retained_from_line, Some(4));
    let first_reconciled_at = first.reconciled_at;
    let first_ended_at = first.ended_at;
    let (first_terminal_observed_at, first_revision) = {
        let inner = registry_b.inner.lock().await;
        let record = inner.jobs_by_id.get(&job.job_id).unwrap();
        (
            record
                .observation.terminal_observed_at
                .expect("terminal inventory is observed by the Server"),
            record
                .observation.revision
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    };

    register(&registry_b, INSTANCE_A, inventory.clone()).await;
    let replayed = registry_b.get_job(&job.job_id).await.unwrap();
    assert_eq!(replayed.status, "completed");
    assert_eq!(replayed.ended_at, first_ended_at);
    assert_eq!(replayed.reconciled_at, first_reconciled_at);
    {
        let inner = registry_b.inner.lock().await;
        let record = inner.jobs_by_id.get(&job.job_id).unwrap();
        assert_eq!(
            record.observation.terminal_observed_at,
            Some(first_terminal_observed_at),
            "terminal inventory replay must not extend Server retention"
        );
        assert_eq!(
            record
                .observation.revision
                .load(std::sync::atomic::Ordering::Relaxed),
            first_revision,
            "idempotent terminal replay must not publish a new revision"
        );
    }
    {
        let mut inner = registry_b.inner.lock().await;
        inner
            .jobs_by_id
            .get_mut(&job.job_id)
            .unwrap()
            .observation.terminal_observed_at = Some(now_ts() - JOB_TERMINAL_RETENTION_SECS);
    }
    let aged_observation = {
        let inner = registry_b.inner.lock().await;
        inner.jobs_by_id[&job.job_id].observation.terminal_observed_at
    };
    register(&registry_b, INSTANCE_A, inventory).await;
    {
        let inner = registry_b.inner.lock().await;
        assert_eq!(
            inner.jobs_by_id[&job.job_id].observation.terminal_observed_at, aged_observation,
            "replay at the retention boundary must not re-anchor the deadline"
        );
    }
    let (_, stdout, _, next, _) = registry_b
        .job_log(&job.job_id, Some(1), None, None)
        .await
        .unwrap();
    assert_eq!(stdout.as_deref(), Some("offline output\n"));
    assert_eq!(next, 5);

    recovery_timeout_sweep(&registry_b).await;
    assert!(registry_b.get_job(&job.job_id).await.is_err());
}

#[tokio::test]
async fn terminal_observed_future_inventory_ended_at_cannot_bypass_prune() {
    let registry_a = RunnerRegistry::default();
    register(&registry_a, INSTANCE_A, empty_inventory()).await;
    let (job, request) = start_and_take_over(&registry_a, INSTANCE_A).await;
    let mut snapshot = snapshot_from_request(
        &job,
        &request,
        "completed",
        7,
        stream("future clock output\n", 1, false),
    );
    let future_ended_at = now_ts() + JOB_TERMINAL_RETENTION_SECS * 100;
    snapshot.ended_at = Some(future_ended_at);
    let inventory = ShellJobInventory {
        active_complete: true,
        jobs: vec![snapshot],
    };

    let registry_b = RunnerRegistry::default();
    let before_register = now_ts();
    register(&registry_b, INSTANCE_A, inventory).await;
    let after_register = now_ts();
    let restored = registry_b.get_job(&job.job_id).await.unwrap();
    assert_eq!(restored.status, "completed");
    assert_eq!(restored.ended_at, Some(future_ended_at));
    let observed_at = {
        let inner = registry_b.inner.lock().await;
        inner.jobs_by_id[&job.job_id]
            .observation.terminal_observed_at
            .expect("terminal inventory observation time")
    };
    assert!((before_register..=after_register).contains(&observed_at));
    assert_ne!(observed_at, future_ended_at);

    let original_request_id = request.request_id.clone();
    let control_request_id = format!("control-{}", job.job_id);
    let control_request = RunnerRequest::from_operation(
        RunnerInvocationMetadata {
            request_id: control_request_id.clone(),
            client_id: request.client_id.clone(),
            requested_by: request.requested_by.clone(),
            created_at: request.created_at,
        },
        RunnerOperation::Job(RunnerJobOperation::Stop {
            job_id: job.job_id.clone(),
        }),
    )
    .unwrap();
    let (persistent_tx, _persistent_rx) = tokio::sync::oneshot::channel::<PersistentShellResult>();
    {
        let mut inner = registry_b.inner.lock().await;
        inner
            .request_to_job
            .insert(original_request_id.clone(), job.job_id.clone());
        inner.pending_by_id.insert(
            original_request_id.clone(),
            PendingShellRequest {
                request: request.clone(),
                operation: request.decode_operation().unwrap(),
                waiter: None,
                job_id: Some(job.job_id.clone()),
                expected_runner_owner: None,
                expected_project_id: None,
                expected_project_cwd: None,
                expected_mcp_gateway_runner_instance_id: None,
                expected_ssh_resource_runner_instance_id: None,
                expected_runner_config_runner_instance_id: None,
                skill_store_fence: None,
                dispatched: true,
                expected_mcp_gateway_provider_id: None,
                expected_mcp_gateway_provider_instance_id: None,
            },
        );
        inner
            .persistent_waiters
            .insert(original_request_id.clone(), persistent_tx);
        inner.pending_by_id.insert(
            control_request_id.clone(),
            PendingShellRequest {
                request: control_request.clone(),
                operation: control_request.decode_operation().unwrap(),
                waiter: None,
                job_id: Some(job.job_id.clone()),
                expected_runner_owner: None,
                expected_project_id: None,
                expected_project_cwd: None,
                expected_mcp_gateway_runner_instance_id: None,
                expected_ssh_resource_runner_instance_id: None,
                expected_runner_config_runner_instance_id: None,
                skill_store_fence: None,
                dispatched: false,
                expected_mcp_gateway_provider_id: None,
                expected_mcp_gateway_provider_instance_id: None,
            },
        );
        let queue = inner
            .queues_by_runner
            .entry(CLIENT_ID.to_string())
            .or_default();
        queue.push_back(original_request_id.clone());
        queue.push_back(control_request_id.clone());
        inner
            .jobs_by_id
            .get_mut(&job.job_id)
            .unwrap()
            .observation.terminal_observed_at = Some(now_ts() - JOB_TERMINAL_RETENTION_SECS);
    }
    registry_b.record_hidden_cleanup_intent(job.job_id.clone(), None);

    recovery_timeout_sweep(&registry_b).await;

    assert!(registry_b.list_jobs(Some(10)).await.is_empty());
    assert!(registry_b.get_job(&job.job_id).await.is_err());
    assert!(registry_b
        .job_log(&job.job_id, None, None, None)
        .await
        .is_err());
    assert!(!registry_b.has_hidden_cleanup_intent_for_test(&job.job_id));
    let inner = registry_b.inner.lock().await;
    assert!(!inner.jobs_by_id.contains_key(&job.job_id));
    assert!(!inner.request_to_job.contains_key(&original_request_id));
    assert!(!inner.request_to_job.values().any(|id| id == &job.job_id));
    assert!(!inner.pending_by_id.contains_key(&original_request_id));
    assert!(!inner.pending_by_id.contains_key(&control_request_id));
    assert!(!inner.persistent_waiters.contains_key(&original_request_id));
    assert!(inner.queues_by_runner.get(CLIENT_ID).is_none_or(|queue| {
        !queue.contains(&original_request_id) && !queue.contains(&control_request_id)
    }));
}

#[tokio::test]
async fn terminal_observed_completed_job_is_retained_then_pruned() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&registry, INSTANCE_A).await;
    registry
        .update_job(update(
            INSTANCE_A,
            &job.job_id,
            1,
            "completed",
            Some("done\n"),
            true,
        ))
        .await
        .unwrap();

    let observed_at = {
        let inner = registry.inner.lock().await;
        inner.jobs_by_id[&job.job_id]
            .observation.terminal_observed_at
            .expect("normal completed job has Server observation time")
    };
    assert!(observed_at <= now_ts());
    recovery_timeout_sweep(&registry).await;
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().status,
        "completed"
    );
    assert!(registry
        .list_jobs(Some(10))
        .await
        .iter()
        .any(|listed| listed.job_id == job.job_id));
    let (_, stdout, _, _, _) = registry
        .job_log(&job.job_id, Some(1), None, None)
        .await
        .unwrap();
    assert_eq!(stdout.as_deref(), Some("done\n"));

    {
        let mut inner = registry.inner.lock().await;
        inner
            .jobs_by_id
            .get_mut(&job.job_id)
            .unwrap()
            .observation.terminal_observed_at = Some(now_ts() - JOB_TERMINAL_RETENTION_SECS);
    }
    recovery_timeout_sweep(&registry).await;
    assert!(!registry
        .list_jobs(Some(10))
        .await
        .iter()
        .any(|listed| listed.job_id == job.job_id));
    assert!(registry.get_job(&job.job_id).await.is_err());
    assert!(registry
        .job_log(&job.job_id, None, None, None)
        .await
        .is_err());
    let inner = registry.inner.lock().await;
    assert!(!inner.request_to_job.values().any(|id| id == &job.job_id));
    assert!(inner
        .pending_by_id
        .values()
        .all(|pending| pending.job_id.as_deref() != Some(job.job_id.as_str())));
}

#[tokio::test]
async fn terminal_observed_hidden_until_handoff_is_not_pruned_by_public_retention_sweep() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&registry, INSTANCE_A).await;
    {
        let mut inner = registry.inner.lock().await;
        inner.jobs_by_id.get_mut(&job.job_id).unwrap().visibility =
            ShellJobVisibility::HiddenUntilHandoff;
    }
    registry
        .update_job(update(INSTANCE_A, &job.job_id, 1, "completed", None, true))
        .await
        .unwrap();
    {
        let mut inner = registry.inner.lock().await;
        let record = inner.jobs_by_id.get_mut(&job.job_id).unwrap();
        assert_eq!(record.visibility, ShellJobVisibility::HiddenUntilHandoff);
        record.observation.terminal_observed_at = Some(now_ts() - JOB_TERMINAL_RETENTION_SECS);
    }

    recovery_timeout_sweep(&registry).await;

    let inner = registry.inner.lock().await;
    let record = inner
        .jobs_by_id
        .get(&job.job_id)
        .expect("hidden terminal jobs use the hidden cleanup lifecycle");
    assert_eq!(record.visibility, ShellJobVisibility::HiddenUntilHandoff);
    assert_eq!(record.public_status(), "completed");
}

#[tokio::test]
async fn terminal_observed_missing_internal_time_is_backfilled_before_prune() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&registry, INSTANCE_A).await;
    registry
        .update_job(update(INSTANCE_A, &job.job_id, 1, "completed", None, true))
        .await
        .unwrap();
    let ancient_ended_at = now_ts() - JOB_TERMINAL_RETENTION_SECS * 100;
    let revision_before = {
        let mut inner = registry.inner.lock().await;
        let record = inner.jobs_by_id.get_mut(&job.job_id).unwrap();
        record.ended_at = Some(ancient_ended_at);
        record.observation.terminal_observed_at = None;
        record
            .observation.revision
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    let before_sweep = now_ts();

    recovery_timeout_sweep(&registry).await;

    let after_sweep = now_ts();
    {
        let inner = registry.inner.lock().await;
        let record = inner
            .jobs_by_id
            .get(&job.job_id)
            .expect("missing observation is initialized, not immediately pruned");
        let observed_at = record.observation.terminal_observed_at.unwrap();
        assert!((before_sweep..=after_sweep).contains(&observed_at));
        assert_eq!(record.ended_at, Some(ancient_ended_at));
        assert_eq!(
            record
                .observation.revision
                .load(std::sync::atomic::Ordering::Relaxed),
            revision_before,
            "internal lifecycle backfill is not a public Job mutation"
        );
    }
    recovery_timeout_sweep(&registry).await;
    assert!(registry.get_job(&job.job_id).await.is_ok());
    {
        let mut inner = registry.inner.lock().await;
        inner
            .jobs_by_id
            .get_mut(&job.job_id)
            .unwrap()
            .observation.terminal_observed_at = Some(now_ts() - JOB_TERMINAL_RETENTION_SECS);
    }
    recovery_timeout_sweep(&registry).await;
    assert!(registry.get_job(&job.job_id).await.is_err());
}

#[tokio::test]
async fn terminal_observed_sequenced_terminal_classes_are_recorded_once() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;

    for status in [
        "completed",
        "failed",
        "stopped",
        "timeout",
        "timed_out",
        "cancelled",
        "lost",
    ] {
        let (job, _) = start_and_take_over(&registry, INSTANCE_A).await;
        registry
            .update_job(update(INSTANCE_A, &job.job_id, 1, status, None, true))
            .await
            .unwrap();
        let first = registry.get_job(&job.job_id).await.unwrap();
        let (observed_at, revision) = {
            let inner = registry.inner.lock().await;
            let record = inner.jobs_by_id.get(&job.job_id).unwrap();
            (
                record
                    .observation.terminal_observed_at
                    .expect("terminal update has Server observation time"),
                record
                    .observation.revision
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
        };
        let late_status = if status == "completed" {
            "failed"
        } else {
            "completed"
        };
        registry
            .update_job(update(
                INSTANCE_A,
                &job.job_id,
                2,
                late_status,
                Some("late\n"),
                true,
            ))
            .await
            .unwrap();
        let replayed = registry.get_job(&job.job_id).await.unwrap();
        assert_eq!(replayed.status, first.status);
        assert_eq!(replayed.ended_at, first.ended_at);
        assert_eq!(replayed.error, first.error);
        assert_eq!(replayed.recovery_reason_code, first.recovery_reason_code);
        assert_eq!(replayed.last_update_seq, first.last_update_seq);
        let inner = registry.inner.lock().await;
        let record = inner.jobs_by_id.get(&job.job_id).unwrap();
        assert_eq!(record.observation.terminal_observed_at, Some(observed_at));
        assert_eq!(
            record
                .observation.revision
                .load(std::sync::atomic::Ordering::Relaxed),
            revision
        );
    }
}

#[tokio::test]
async fn job_reconciliation_same_instance_replaces_tail_without_duplicates() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, request) = start_and_take_over(&registry, INSTANCE_A).await;
    registry
        .update_job(update(
            INSTANCE_A,
            &job.job_id,
            1,
            "running",
            Some("one\n"),
            false,
        ))
        .await
        .unwrap();
    registry.reconcile_disconnect(CLIENT_ID, INSTANCE_A).await;
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().status,
        "recovering"
    );

    {
        let inner = registry.inner.lock().await;
        let record = inner.jobs_by_id.get(&job.job_id).unwrap();
        assert_eq!(record.lifecycle, JobLifecycleState::Running);
        assert_eq!(record.recovery.phase, Some(JobRecoveryPhase::Recovering));
    }

    let reconciled =
        snapshot_from_request(&job, &request, "running", 2, stream("one\ntwo\n", 1, false));
    register(
        &registry,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![reconciled],
        },
    )
    .await;
    let running = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(running.status, "running");
    assert_eq!(running.recovery_state.as_deref(), Some("reconciled"));
    assert_eq!(
        running.recovery_reason_code.as_deref(),
        Some("same_instance_reconciliation")
    );

    {
        let inner = registry.inner.lock().await;
        let record = inner.jobs_by_id.get(&job.job_id).unwrap();
        assert_eq!(record.lifecycle, JobLifecycleState::Running);
        assert_eq!(record.recovery.phase, Some(JobRecoveryPhase::Reconciled));
    }

    registry
        .update_job(update(
            INSTANCE_A,
            &job.job_id,
            1,
            "running",
            Some("one\n"),
            false,
        ))
        .await
        .unwrap();
    registry
        .update_job(update(
            INSTANCE_A,
            &job.job_id,
            3,
            "running",
            Some("three\n"),
            false,
        ))
        .await
        .unwrap();
    let (_, stdout, _, next, _) = registry
        .job_log(&job.job_id, Some(1), None, None)
        .await
        .unwrap();
    assert_eq!(stdout.as_deref(), Some("one\ntwo\nthree\n"));
    assert_eq!(next, 4);

    let authoritative = snapshot_from_request(
        &job,
        &request,
        "running",
        4,
        stream("two\nthree\nfour\n", 2, true),
    );
    register(
        &registry,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![authoritative],
        },
    )
    .await;
    let (_, stdout, _, next, _) = registry
        .job_log(&job.job_id, Some(1), None, None)
        .await
        .unwrap();
    assert_eq!(stdout.as_deref(), Some("two\nthree\nfour\n"));
    assert_eq!(next, 5);
    let status = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(status.stdout_retained_from_line, Some(2));
    assert_eq!(status.last_update_seq, Some(4));

    let mut replay = update(INSTANCE_A, &job.job_id, 5, "running", None, false);
    replay.log_snapshot = Some(ShellJobLogSnapshot {
        stdout: stream("two\nthree\nfour\nfive\n", 2, true),
        stderr: ShellJobStreamSnapshot::default(),
    });
    registry.update_job(replay).await.unwrap();
    let mut stale_replay = update(INSTANCE_A, &job.job_id, 4, "running", None, false);
    stale_replay.log_snapshot = Some(ShellJobLogSnapshot {
        stdout: stream("two\nthree\nfour\n", 2, true),
        stderr: ShellJobStreamSnapshot::default(),
    });
    registry.update_job(stale_replay).await.unwrap();
    let (_, stdout, _, next, _) = registry
        .job_log(&job.job_id, Some(1), None, None)
        .await
        .unwrap();
    assert_eq!(stdout.as_deref(), Some("two\nthree\nfour\nfive\n"));
    assert_eq!(next, 6);

    let mut cursor_regression = update(INSTANCE_A, &job.job_id, 6, "running", None, false);
    cursor_regression.log_snapshot = Some(ShellJobLogSnapshot {
        stdout: ShellJobStreamSnapshot::default(),
        stderr: ShellJobStreamSnapshot::default(),
    });
    assert!(registry
        .update_job(cursor_regression)
        .await
        .unwrap_err()
        .contains("regresses an absolute cursor"));
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().last_update_seq,
        Some(5)
    );
}

#[tokio::test]
async fn job_reconciliation_same_instance_stale_connection_disconnect_is_noop() {
    let registry = RunnerRegistry::default();
    registry
        .register_streaming_session(
            register_request(INSTANCE_A, empty_inventory()),
            None,
            "connection-a",
            super::RunnerTransport::WebSocket,
            std::sync::Arc::new(tokio::sync::Notify::new()),
        )
        .await
        .unwrap();
    let (job, request) = start_and_take_over(&registry, INSTANCE_A).await;
    let snapshot = snapshot_from_request(
        &job,
        &request,
        "running",
        1,
        ShellJobStreamSnapshot::default(),
    );
    registry
        .register_streaming_session(
            register_request(
                INSTANCE_A,
                ShellJobInventory {
                    active_complete: true,
                    jobs: vec![snapshot],
                },
            ),
            None,
            "connection-b",
            super::RunnerTransport::WebSocket,
            std::sync::Arc::new(tokio::sync::Notify::new()),
        )
        .await
        .unwrap();

    registry
        .reconcile_disconnect_for_connection(CLIENT_ID, INSTANCE_A, "connection-a")
        .await;
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().status,
        "running"
    );
    assert!(registry
        .get_runner_view_for_connection(CLIENT_ID, INSTANCE_A, "connection-b")
        .await
        .is_some());

    registry
        .reconcile_disconnect_for_connection(CLIENT_ID, INSTANCE_A, "connection-b")
        .await;
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().status,
        "recovering"
    );
}

#[tokio::test]
async fn job_reconciliation_instance_replacement_fences_old_runner() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&registry, INSTANCE_A).await;
    registry
        .update_job(update(INSTANCE_A, &job.job_id, 1, "running", None, false))
        .await
        .unwrap();
    registry.reconcile_disconnect(CLIENT_ID, INSTANCE_A).await;

    register(&registry, INSTANCE_B, empty_inventory()).await;
    let lost = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(lost.status, "lost");
    assert_eq!(
        lost.recovery_reason_code.as_deref(),
        Some("runner_instance_replaced")
    );
    assert!(registry
        .update_job(update(INSTANCE_A, &job.job_id, 2, "running", None, false,))
        .await
        .unwrap_err()
        .contains("no longer the active instance"));
    assert!(registry
        .register(register_request(INSTANCE_A, empty_inventory()))
        .await
        .unwrap_err()
        .contains("instance was replaced"));
    assert_eq!(
        registry
            .get_runner_view(CLIENT_ID)
            .await
            .unwrap()
            .pending_requests,
        0
    );
}

#[tokio::test]
async fn job_reconciliation_instance_replacement_does_not_redispatch_server_queue() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let queued = registry
        .start_job(start_request("echo must-not-run"), "tester".to_string())
        .await
        .unwrap();
    assert_eq!(queued.status, "queued");
    registry
        .set_last_seen_for_test(CLIENT_ID, now_ts() - 120)
        .await;

    register(&registry, INSTANCE_B, empty_inventory()).await;
    let lost = registry.get_job(&queued.job_id).await.unwrap();
    assert_eq!(lost.status, "lost");
    assert_eq!(
        lost.recovery_reason_code.as_deref(),
        Some("runner_instance_replaced")
    );
    assert!(registry
        .poll(RunnerPollRequest {
            client_id: CLIENT_ID.to_string(),
            runner_instance_id: INSTANCE_B.to_string(),
        })
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn job_reconciliation_complete_inventory_missing_marks_job_lost() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&registry, INSTANCE_A).await;
    registry
        .update_job(update(INSTANCE_A, &job.job_id, 1, "running", None, false))
        .await
        .unwrap();
    registry.reconcile_disconnect(CLIENT_ID, INSTANCE_A).await;

    register(&registry, INSTANCE_A, empty_inventory()).await;
    let lost = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(lost.status, "lost");
    assert_eq!(
        lost.recovery_reason_code.as_deref(),
        Some("runner_inventory_missing")
    );
    let inner = registry.inner.lock().await;
    assert!(inner.pending_by_id.is_empty());
    assert!(inner.request_to_job.is_empty());
    assert!(inner
        .queues_by_runner
        .get(CLIENT_ID)
        .is_none_or(|queue| queue.is_empty()));
}

#[tokio::test]
async fn job_reconciliation_recovery_deadline_and_unavailable_stop_are_explicit() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, request) = start_and_take_over(&registry, INSTANCE_A).await;
    registry
        .update_job(update(INSTANCE_A, &job.job_id, 1, "running", None, false))
        .await
        .unwrap();
    registry.reconcile_disconnect(CLIENT_ID, INSTANCE_A).await;
    let recovering = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(recovering.status, "recovering");
    assert!(recovering.ended_at.is_none());
    let stop_error = registry
        .stop_job(&job.job_id, "tester".to_string())
        .await
        .unwrap_err();
    assert!(stop_error.contains("runner_unavailable_recovering"));

    {
        let mut inner = registry.inner.lock().await;
        inner
            .jobs_by_id
            .get_mut(&job.job_id)
            .unwrap()
            .recovery
            .recovering_since = Some(now_ts() - JOB_RECOVERY_GRACE_SECS);
    }
    let late_snapshot = snapshot_from_request(
        &job,
        &request,
        "running",
        2,
        ShellJobStreamSnapshot::default(),
    );
    register(
        &registry,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![late_snapshot],
        },
    )
    .await;
    let lost = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(lost.status, "lost");
    assert_eq!(
        lost.recovery_reason_code.as_deref(),
        Some("runner_recovery_deadline_exceeded")
    );
    let first_ended_at = lost.ended_at;
    assert!(first_ended_at.is_some());
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().ended_at,
        first_ended_at
    );
}

#[tokio::test]
async fn job_reconciliation_stop_restored_job_targets_original_id() {
    let registry_a = RunnerRegistry::default();
    register(&registry_a, INSTANCE_A, empty_inventory()).await;
    let (job, request) = start_and_take_over(&registry_a, INSTANCE_A).await;
    let snapshot = snapshot_from_request(
        &job,
        &request,
        "running",
        4,
        ShellJobStreamSnapshot::default(),
    );
    let registry_b = RunnerRegistry::default();
    register(
        &registry_b,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![snapshot],
        },
    )
    .await;

    let requested = registry_b
        .stop_job(&job.job_id, "tester".to_string())
        .await
        .unwrap();
    assert_eq!(requested.status, "stop_requested");
    let stop = registry_b
        .poll(RunnerPollRequest {
            client_id: CLIENT_ID.to_string(),
            runner_instance_id: INSTANCE_A.to_string(),
        })
        .await
        .unwrap()
        .expect("stop request");
    assert_eq!(stop.kind, "stop_job");
    assert_eq!(stop.job_id.as_deref(), Some(job.job_id.as_str()));
    registry_b
        .update_job(update(INSTANCE_A, &job.job_id, 5, "stopped", None, true))
        .await
        .unwrap();
    assert_eq!(
        registry_b.get_job(&job.job_id).await.unwrap().status,
        "stopped"
    );
    assert_eq!(registry_b.list_jobs(Some(10)).await.len(), 1);
}

fn standalone_snapshot(job_id: &str, status: &str) -> ShellJobSnapshot {
    let terminal = status == "completed";
    ShellJobSnapshot {
        job_id: job_id.to_string(),
        request_id: format!("request-{job_id}"),
        status: status.to_string(),
        update_seq: 1,
        created_at: 1_700_000_000,
        started_at: Some(1_700_000_001),
        ended_at: terminal.then_some(1_700_000_002),
        exit_code: terminal.then_some(0),
        duration_ms: terminal.then_some(1_000),
        error: None,
        command_execution_state: None,
        context: ShellJobContext {
            runtime_project_id: None,
            workflow_session_id: None,
            ssh_resource: None,
            project_cwd: None,
            cwd: Some("/tmp".to_string()),
            purpose: Some("test".to_string()),
            shell: Some("bash".to_string()),
            command_preview: "safe preview".to_string(),
            validation_steps: Vec::new(),
            validation: None,
            structured_execution: None,
        },
        stdout: ShellJobStreamSnapshot::default(),
        stderr: ShellJobStreamSnapshot::default(),
        validation_progress: None,
        activity: None,
    }
}

#[test]
fn job_inventory_accepts_javascript_structured_script_context() {
    let mut javascript = standalone_snapshot("javascript-running", "running");
    javascript.context.shell = Some("javascript".to_string());
    javascript.context.command_preview = "javascript script (24 bytes, 1 args)".to_string();
    javascript.context.structured_execution = Some(
        crate::runner_protocol::ShellJobStructuredExecutionMetadata {
            execution_source: "run_script".to_string(),
            language: Some(ShellScriptLanguage::Javascript),
            script_bytes: Some(24),
            arg_count: 1,
            stdin_present: false,
            validation_identity: None,
            validation_tool: None,
            assertion_name: None,
        },
    );
    let inventory = ShellJobInventory {
        active_complete: true,
        jobs: vec![javascript.clone()],
    };
    validate_job_inventory(CLIENT_ID, &[project_summary()], &inventory).unwrap();

    javascript.context.shell = Some("node".to_string());
    let error = validate_job_inventory(
        CLIENT_ID,
        &[project_summary()],
        &ShellJobInventory {
            active_complete: true,
            jobs: vec![javascript],
        },
    )
    .unwrap_err();
    assert!(error.contains("shell is invalid"), "{error}");
}

#[test]
fn job_inventory_accepts_typescript_semantic_identity_and_rejects_runtime_identity() {
    let snapshot = || {
        let mut typescript = standalone_snapshot("typescript-running", "running");
        typescript.context.shell = Some("typescript".to_string());
        typescript.context.command_preview = "typescript script (24 bytes, 1 args)".to_string();
        typescript.context.structured_execution = Some(
            crate::runner_protocol::ShellJobStructuredExecutionMetadata {
                execution_source: "run_script".to_string(),
                language: Some(ShellScriptLanguage::Typescript),
                script_bytes: Some(24),
                arg_count: 1,
                stdin_present: false,
                validation_identity: None,
                validation_tool: None,
                assertion_name: None,
            },
        );
        typescript
    };
    validate_job_inventory(
        CLIENT_ID,
        &[project_summary()],
        &ShellJobInventory {
            active_complete: true,
            jobs: vec![snapshot()],
        },
    )
    .unwrap();

    for concrete_runtime in ["node", "tsx"] {
        let mut invalid = snapshot();
        invalid.context.shell = Some(concrete_runtime.to_string());
        let error = validate_job_inventory(
            CLIENT_ID,
            &[project_summary()],
            &ShellJobInventory {
                active_complete: true,
                jobs: vec![invalid],
            },
        )
        .unwrap_err();
        assert!(error.contains("shell is invalid"), "{error}");
    }
}

#[tokio::test]
async fn fresh_server_reconstructs_agent_queued_job_with_same_identity() {
    let registry = RunnerRegistry::default();
    let mut queued = standalone_snapshot("queued-across-server-restart", "agent_queued");
    queued.started_at = None;
    queued.context.runtime_project_id = Some(RUNTIME_PROJECT_ID.to_string());
    queued.context.workflow_session_id = Some(SESSION_ID.to_string());

    register(
        &registry,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![queued],
        },
    )
    .await;

    let recovered = registry
        .get_job("queued-across-server-restart")
        .await
        .unwrap();
    assert_eq!(recovered.job_id, "queued-across-server-restart");
    assert_eq!(recovered.status, "agent_queued");
    assert_eq!(recovered.project_id.as_deref(), Some(RUNTIME_PROJECT_ID));
    assert_eq!(recovered.session_id.as_deref(), Some(SESSION_ID));
    assert!(recovered.recovered_after_server_restart);
    assert_eq!(
        recovered.recovery_reason_code.as_deref(),
        Some("server_restart_reconciliation")
    );
    assert_eq!(recovered.command_execution_state, None);
}

#[tokio::test]
async fn reconciliation_summary_counts_inventory_effects_without_payload_data() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;

    let first_inventory = ShellJobInventory {
        active_complete: true,
        jobs: vec![
            standalone_snapshot("summary-active-a", "running"),
            standalone_snapshot("summary-active-b", "running"),
        ],
    };
    {
        let mut inner = registry.inner.lock().await;
        let first = reconcile_inventory_locked(
            &mut inner,
            CLIENT_ID,
            INSTANCE_A,
            None,
            registry.observation_epoch.clone(),
            &first_inventory,
            now_ts(),
        );
        assert_eq!(first.inventory_active, 2);
        assert_eq!(first.inventory_terminal, 0);
        assert_eq!(first.reconstructed, 2);
        assert_eq!(first.updated, 0);
        assert_eq!(first.missing, 0);
        assert_eq!(first.suppressed_terminal, 0);
    }

    let mut updated = standalone_snapshot("summary-active-a", "running");
    updated.update_seq = 2;
    let second_inventory = ShellJobInventory {
        active_complete: true,
        jobs: vec![
            updated,
            standalone_snapshot("summary-terminal", "completed"),
        ],
    };
    let mut inner = registry.inner.lock().await;
    let second = reconcile_inventory_locked(
        &mut inner,
        CLIENT_ID,
        INSTANCE_A,
        None,
        registry.observation_epoch.clone(),
        &second_inventory,
        now_ts(),
    );
    assert_eq!(second.inventory_active, 1);
    assert_eq!(second.inventory_terminal, 1);
    assert_eq!(second.reconstructed, 1);
    assert_eq!(second.updated, 1);
    assert_eq!(second.missing, 1);
    assert_eq!(second.suppressed_terminal, 0);
}

#[tokio::test]
async fn paged_project_registration_does_not_make_active_job_inventory_a_liveness_fence() {
    let registry = RunnerRegistry::default();
    let mut snapshot = standalone_snapshot("paged-active-job", "running");
    snapshot.context.runtime_project_id = Some(RUNTIME_PROJECT_ID.to_string());
    let request = register_request(
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![snapshot],
        },
    );

    let view = registry.register(request).await.expect(
        "paged project inventory must not reject Runner liveness during job reconciliation",
    );
    assert!(view.connected);
    assert!(view.projects.is_empty());
    assert_eq!(
        view.project_inventory
            .as_ref()
            .map(|status| status.sync_state.as_str()),
        Some("pending")
    );
    assert_eq!(registry.list_jobs(Some(10)).await.len(), 1);
}

#[test]
fn job_reconciliation_inventory_validation_is_bounded_and_atomic() {
    let projects = vec![project_summary()];
    let duplicate = standalone_snapshot("duplicate-job", "running");
    let error = validate_job_inventory(
        CLIENT_ID,
        &projects,
        &ShellJobInventory {
            active_complete: true,
            jobs: vec![duplicate.clone(), duplicate],
        },
    )
    .unwrap_err();
    assert!(error.contains("duplicate job_id"));

    let mut invalid_status = standalone_snapshot("bad-status", "running");
    invalid_status.status = "mystery".to_string();
    assert!(validate_job_inventory(
        CLIENT_ID,
        &projects,
        &ShellJobInventory {
            active_complete: true,
            jobs: vec![invalid_status],
        },
    )
    .unwrap_err()
    .contains("status"));

    let mut incomplete_validation = standalone_snapshot("incomplete-validation", "completed");
    incomplete_validation.context.validation_steps = vec!["check".to_string()];
    incomplete_validation.validation_progress = Some(ShellJobValidationProgress {
        completed: 0,
        current_step: None,
        failed_step: None,
    });
    assert!(validate_job_inventory(
        CLIENT_ID,
        &projects,
        &ShellJobInventory {
            active_complete: true,
            jobs: vec![incomplete_validation],
        },
    )
    .unwrap_err()
    .contains("does not match status"));

    let mut oversized_stream = standalone_snapshot("oversized-stream", "running");
    oversized_stream.stdout = stream(&"x".repeat(JOB_SNAPSHOT_STREAM_MAX_BYTES + 1), 1, true);
    assert!(validate_job_inventory(
        CLIENT_ID,
        &projects,
        &ShellJobInventory {
            active_complete: true,
            jobs: vec![oversized_stream],
        },
    )
    .unwrap_err()
    .contains("stdout exceeds"));

    let too_many_terminal = (0..=JOB_INVENTORY_MAX_TERMINAL_JOBS)
        .map(|index| standalone_snapshot(&format!("terminal-{index}"), "completed"))
        .collect();
    assert!(validate_job_inventory(
        CLIENT_ID,
        &projects,
        &ShellJobInventory {
            active_complete: true,
            jobs: too_many_terminal,
        },
    )
    .unwrap_err()
    .contains("terminal records"));

    let large_tail = "x\n".repeat(JOB_SNAPSHOT_STREAM_MAX_BYTES / 2);
    let large_inventory = ShellJobInventory {
        active_complete: true,
        jobs: (0..JOB_INVENTORY_MAX_TERMINAL_JOBS)
            .map(|index| {
                let mut snapshot =
                    standalone_snapshot(&format!("large-terminal-{index}"), "completed");
                snapshot.stdout = stream(&large_tail, 1, false);
                snapshot.stderr = stream(&large_tail, 1, false);
                snapshot
            })
            .collect(),
    };
    assert!(
        validate_job_inventory(CLIENT_ID, &projects, &large_inventory)
            .unwrap_err()
            .contains("serialized bytes")
    );

    let mut wrong_project = standalone_snapshot("wrong-project", "running");
    wrong_project.context.runtime_project_id = Some("agent:oe:missing".to_string());
    assert!(validate_job_inventory(
        CLIENT_ID,
        &projects,
        &ShellJobInventory {
            active_complete: true,
            jobs: vec![wrong_project],
        },
    )
    .unwrap_err()
    .contains("not registered"));

    let mut deferred_project = standalone_snapshot("deferred-project", "running");
    deferred_project.context.runtime_project_id = Some(RUNTIME_PROJECT_ID.to_string());
    let deferred_inventory = ShellJobInventory {
        active_complete: true,
        jobs: vec![deferred_project.clone()],
    };
    assert!(validate_job_inventory(CLIENT_ID, &[], &deferred_inventory)
        .unwrap_err()
        .contains("not registered"));
    assert!(
        validate_job_inventory_without_project_membership(CLIENT_ID, &deferred_inventory).is_ok()
    );

    deferred_project.context.runtime_project_id = Some("agent:other:demo".to_string());
    assert!(validate_job_inventory_without_project_membership(
        CLIENT_ID,
        &ShellJobInventory {
            active_complete: true,
            jobs: vec![deferred_project],
        },
    )
    .unwrap_err()
    .contains("does not belong to client_id"));

    let mut invalid_session = standalone_snapshot("invalid-session", "running");
    invalid_session.context.runtime_project_id = Some(RUNTIME_PROJECT_ID.to_string());
    invalid_session.context.workflow_session_id = Some("foreign-session".to_string());
    assert!(validate_job_inventory(
        CLIENT_ID,
        &projects,
        &ShellJobInventory {
            active_complete: true,
            jobs: vec![invalid_session],
        },
    )
    .unwrap_err()
    .contains("workflow_session_id"));

    let safe = standalone_snapshot("no-raw-command", "running");
    let encoded = serde_json::to_value(&safe).unwrap();
    let snapshot = encoded.as_object().unwrap();
    for forbidden_field in ["command", "raw_command", "stdin", "env", "token", "config"] {
        assert!(!snapshot.contains_key(forbidden_field));
        assert!(!snapshot["context"]
            .as_object()
            .unwrap()
            .contains_key(forbidden_field));
    }
    assert!(serde_json::to_vec(&encoded).unwrap().len() < LIVE_JOB_STREAM_RETENTION_BYTES);
}

#[tokio::test]
async fn job_reconciliation_malformed_inventory_does_not_mutate_registry() {
    let registry = RunnerRegistry::default();
    let duplicate = standalone_snapshot("duplicate-job", "running");
    let mut request = register_request(
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![duplicate.clone(), duplicate],
        },
    );
    request.display_name = Some("must not be installed".to_string());
    assert!(registry.register(request).await.is_err());
    assert!(registry.get_runner_view(CLIENT_ID).await.is_none());
    assert!(registry.list_jobs(Some(10)).await.is_empty());

    register(&registry, INSTANCE_A, empty_inventory()).await;
    let queued = registry
        .start_job(
            start_request("echo request-id-collision"),
            "tester".to_string(),
        )
        .await
        .unwrap();
    let queued_request_id = {
        let inner = registry.inner.lock().await;
        inner
            .jobs_by_id
            .get(&queued.job_id)
            .and_then(|job| job.request_id.clone())
            .unwrap()
    };
    let mut collision = standalone_snapshot("different-job", "completed");
    collision.request_id = queued_request_id.clone();
    assert!(registry
        .register(register_request(
            INSTANCE_A,
            ShellJobInventory {
                active_complete: true,
                jobs: vec![collision],
            },
        ))
        .await
        .unwrap_err()
        .contains("belongs to a different job"));
    let inner = registry.inner.lock().await;
    assert_eq!(
        inner.request_to_job.get(&queued_request_id),
        Some(&queued.job_id)
    );
    assert_eq!(
        inner.jobs_by_id.get(&queued.job_id).unwrap().public_status(),
        "queued"
    );
}

#[tokio::test]
async fn job_reconciliation_absent_capability_keeps_immediate_lost_semantics() {
    let mismatch_registry = RunnerRegistry::default();
    let mut missing_inventory = register_request(INSTANCE_A, empty_inventory());
    missing_inventory.job_inventory = None;
    assert!(mismatch_registry
        .register(missing_inventory)
        .await
        .unwrap_err()
        .contains("requires job_inventory"));
    let mut unexpected_inventory = register_request(INSTANCE_A, empty_inventory());
    unexpected_inventory.capabilities =
        crate::test_support::current_runner_capabilities(RunnerCapabilities::default());
    assert!(mismatch_registry
        .register(unexpected_inventory)
        .await
        .unwrap_err()
        .contains("requires job_state_reconciliation"));
    let downgrade_registry = RunnerRegistry::default();
    register(&downgrade_registry, INSTANCE_A, empty_inventory()).await;
    let mut downgraded = register_request(INSTANCE_A, empty_inventory());
    downgraded.capabilities =
        crate::test_support::current_runner_capabilities(RunnerCapabilities::default());
    downgraded.job_inventory = None;
    assert!(downgrade_registry
        .register(downgraded)
        .await
        .unwrap_err()
        .contains("cannot downgrade"));

    let registry = RunnerRegistry::default();
    let mut request = register_request(INSTANCE_A, empty_inventory());
    request.capabilities =
        crate::test_support::current_runner_capabilities(RunnerCapabilities {
            jobs: true,
            async_jobs: true,
            async_shell_jobs: true,
            ..Default::default()
        });
    request.job_inventory = None;
    registry.register(request).await.unwrap();
    let job = registry
        .start_job(start_request("sleep 30"), "tester".to_string())
        .await
        .unwrap();
    registry
        .poll(RunnerPollRequest {
            client_id: CLIENT_ID.to_string(),
            runner_instance_id: INSTANCE_A.to_string(),
        })
        .await
        .unwrap()
        .unwrap();
    registry.reconcile_disconnect(CLIENT_ID, INSTANCE_A).await;
    let lost = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(lost.status, "lost");
    assert_eq!(
        lost.recovery_reason_code.as_deref(),
        Some("runner_disconnected_without_reconciliation")
    );
}

// ---- Recovery-timeout sweep ----
//
// The sweep closes the gap left by the on-demand deadline check: a
// reconciliation-capable runner that disconnects permanently and is never
// queried again must not stay `recovering` forever. The sweep is
// non-request-triggered and bounded. These tests drive a job into
// `recovering` via the transport-disconnect path, manipulate
// `recovering_since` directly (the existing test idiom), then invoke
// `recovery_timeout_sweep` and assert the terminal transition.

/// Drive the job for `instance` into `recovering` by disconnecting its
/// transport, returning its job_id. Caller must have already started and
/// polled the job so the runner "owns" it.
async fn drive_into_recovering(registry: &RunnerRegistry, job_id: &str, instance: &str) {
    registry
        .update_job(update(instance, job_id, 1, "running", None, false))
        .await
        .unwrap();
    registry.reconcile_disconnect(CLIENT_ID, instance).await;
    assert_eq!(registry.get_job(job_id).await.unwrap().status, "recovering");
}

/// Set a job's `recovering_since` to `now - offset_secs`, simulating the
/// passage of the recovery deadline without sleeping the full grace window.
async fn age_recovering_since(registry: &RunnerRegistry, job_id: &str, offset_secs: i64) {
    let mut inner = registry.inner.lock().await;
    let job = inner.jobs_by_id.get_mut(job_id).expect("job exists");
    job.recovery.recovering_since = Some(now_ts() - offset_secs);
}

#[tokio::test]
async fn clamp_grace_bounds_the_resolved_recovery_window() {
    assert_eq!(clamp_grace(0), 5);
    assert_eq!(clamp_grace(-5), 5);
    assert_eq!(clamp_grace(60), 60);
    assert_eq!(clamp_grace(100_000), 3_600);
    assert_eq!(clamp_grace(120), 120);
}

#[tokio::test]
async fn recovery_sweep_transitions_expired_recovering_job_to_lost() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&registry, INSTANCE_A).await;
    drive_into_recovering(&registry, &job.job_id, INSTANCE_A).await;
    age_recovering_since(&registry, &job.job_id, job_recovery_grace_secs() + 1).await;

    recovery_timeout_sweep(&registry).await;
    let lost = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(lost.status, "lost");
    assert_eq!(lost.recovery_state.as_deref(), Some("lost_after_reconcile"));
    assert_eq!(
        lost.recovery_reason_code.as_deref(),
        Some("runner_recovery_deadline_exceeded")
    );
    assert!(lost.ended_at.is_some(), "expired job records ended_at");
}

#[tokio::test]
async fn cleanup_pending_recovering_job_stays_tracked_until_lost_then_is_removed() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&registry, INSTANCE_A).await;
    drive_into_recovering(&registry, &job.job_id, INSTANCE_A).await;
    {
        let mut inner = registry.inner.lock().await;
        inner
            .jobs_by_id
            .get_mut(&job.job_id)
            .expect("job exists")
            .visibility = ShellJobVisibility::HiddenUntilHandoff;
    }

    assert!(!registry
        .cancel_hidden_job_for_auth(None, &job.job_id)
        .await
        .unwrap());
    {
        let inner = registry.inner.lock().await;
        let retained = inner.jobs_by_id.get(&job.job_id).expect("job retained");
        assert_eq!(retained.public_status(), "recovering");
        assert_eq!(retained.visibility, ShellJobVisibility::CleanupPending);
    }
    assert!(registry.get_job(&job.job_id).await.is_err());

    age_recovering_since(&registry, &job.job_id, job_recovery_grace_secs() + 1).await;
    recovery_timeout_sweep(&registry).await;
    assert!(
        !registry
            .inner
            .lock()
            .await
            .jobs_by_id
            .contains_key(&job.job_id),
        "cleanup-pending record must only disappear after the lost terminal transition"
    );
}

#[tokio::test]
async fn recovery_sweep_noop_before_deadline() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&registry, INSTANCE_A).await;
    drive_into_recovering(&registry, &job.job_id, INSTANCE_A).await;
    // Deadline not yet elapsed (recovery just started).
    age_recovering_since(&registry, &job.job_id, 1).await;

    recovery_timeout_sweep(&registry).await;
    let recovering = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(recovering.status, "recovering");
    assert!(
        recovering.ended_at.is_none(),
        "pre-deadline job stays recovering"
    );
}

#[tokio::test]
async fn terminal_observed_recovery_sweep_is_idempotent() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&registry, INSTANCE_A).await;
    drive_into_recovering(&registry, &job.job_id, INSTANCE_A).await;
    age_recovering_since(&registry, &job.job_id, job_recovery_grace_secs() + 5).await;

    recovery_timeout_sweep(&registry).await;
    let first = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(first.status, "lost");
    let first_ended_at = first.ended_at.expect("ended_at set");
    let (first_terminal_observed_at, first_revision) = {
        let inner = registry.inner.lock().await;
        let record = inner.jobs_by_id.get(&job.job_id).unwrap();
        (
            record
                .observation.terminal_observed_at
                .expect("lost transition has Server observation time"),
            record
                .observation.revision
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    };

    recovery_timeout_sweep(&registry).await;
    let second = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(second.status, "lost");
    assert_eq!(
        second.ended_at,
        Some(first_ended_at),
        "ended_at not rewritten"
    );
    assert_eq!(
        second.recovery_reason_code.as_deref(),
        Some("runner_recovery_deadline_exceeded"),
        "reason not overwritten"
    );
    let inner = registry.inner.lock().await;
    let record = inner.jobs_by_id.get(&job.job_id).unwrap();
    assert_eq!(
        record.observation.terminal_observed_at,
        Some(first_terminal_observed_at),
        "repeated recovery sweep must not extend retention"
    );
    assert_eq!(
        record
            .observation.revision
            .load(std::sync::atomic::Ordering::Relaxed),
        first_revision,
        "no public revision without a public state change"
    );
}

#[tokio::test]
async fn recovery_sweep_skips_terminal_and_already_lost_jobs() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&registry, INSTANCE_A).await;
    registry
        .update_job(update(
            INSTANCE_A,
            &job.job_id,
            1,
            "completed",
            Some("ok\n"),
            true,
        ))
        .await
        .unwrap();
    // Defensively age a terminal job past the deadline; the sweep must not
    // touch it.
    {
        let mut inner = registry.inner.lock().await;
        let record = inner.jobs_by_id.get_mut(&job.job_id).unwrap();
        record.recovery.recovering_since = Some(now_ts() - job_recovery_grace_secs() - 10);
    }
    recovery_timeout_sweep(&registry).await;
    let completed = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(completed.status, "completed");

    // A job already lost with a different reason keeps its original reason.
    let (job2, _) = start_and_take_over(&registry, INSTANCE_A).await;
    drive_into_recovering(&registry, &job2.job_id, INSTANCE_A).await;
    {
        let mut inner = registry.inner.lock().await;
        let record = inner.jobs_by_id.get_mut(&job2.job_id).unwrap();
        super::jobs::mark_job_lost(
            record,
            now_ts(),
            JobRecoveryReason::RunnerInventoryMissing,
            "runner complete active inventory did not contain this job",
        );
    }
    recovery_timeout_sweep(&registry).await;
    let still_lost = registry.get_job(&job2.job_id).await.unwrap();
    assert_eq!(still_lost.status, "lost");
    assert_eq!(
        still_lost.recovery_reason_code.as_deref(),
        Some("runner_inventory_missing"),
        "sweep must not overwrite an existing terminal reason"
    );
}

#[tokio::test]
async fn recovery_sweep_clears_pending_control_requests() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&registry, INSTANCE_A).await;
    drive_into_recovering(&registry, &job.job_id, INSTANCE_A).await;
    age_recovering_since(&registry, &job.job_id, job_recovery_grace_secs() + 1).await;

    recovery_timeout_sweep(&registry).await;
    let inner = registry.inner.lock().await;
    assert!(inner.pending_by_id.is_empty(), "pending_by_id cleared");
    assert!(inner.request_to_job.is_empty(), "request_to_job cleared");
    assert!(inner
        .queues_by_runner
        .get(CLIENT_ID)
        .is_none_or(|queue| queue.is_empty()));
}

#[tokio::test]
async fn recovery_sweep_pass_cap_bounds_a_single_pass() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    // Start more expired recovering jobs than the per-pass cap.
    let count = RECOVERY_SWEEP_PASS_CAP + 6;
    let mut job_ids = Vec::new();
    for _ in 0..count {
        let job = registry
            .start_job(start_request("sleep 30"), "tester".to_string())
            .await
            .unwrap();
        let request = registry
            .poll(RunnerPollRequest {
                client_id: CLIENT_ID.to_string(),
                runner_instance_id: INSTANCE_A.to_string(),
            })
            .await
            .unwrap()
            .expect("start request");
        registry
            .update_job(update(INSTANCE_A, &job.job_id, 1, "running", None, false))
            .await
            .unwrap();
        assert_eq!(request.kind, "start_job");
        job_ids.push(job.job_id);
    }
    registry.reconcile_disconnect(CLIENT_ID, INSTANCE_A).await;
    for job_id in &job_ids {
        age_recovering_since(&registry, job_id, job_recovery_grace_secs() + 1).await;
    }

    recovery_timeout_sweep(&registry).await;
    // Count via the inner record directly: `get_job` runs the on-demand deadline
    // check (`refresh_job_status_locked`), which would transition the remaining
    // expired recovering jobs during the measurement and hide the cap effect.
    let lost_after_first = {
        let inner = registry.inner.lock().await;
        job_ids
            .iter()
            .filter(|id| {
                inner
                    .jobs_by_id
                    .get(id.as_str())
                    .is_some_and(|j| j.public_status() == "lost")
            })
            .count()
    };
    assert_eq!(
        lost_after_first, RECOVERY_SWEEP_PASS_CAP,
        "first pass transitions at most the cap"
    );
    recovery_timeout_sweep(&registry).await;
    let lost_after_second = {
        let inner = registry.inner.lock().await;
        job_ids
            .iter()
            .filter(|id| {
                inner
                    .jobs_by_id
                    .get(id.as_str())
                    .is_some_and(|j| j.public_status() == "lost")
            })
            .count()
    };
    assert_eq!(lost_after_second, count, "second pass completes the rest");
}

#[tokio::test]
async fn stale_keepalive_does_not_extend_recovery_deadline() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&registry, INSTANCE_A).await;
    drive_into_recovering(&registry, &job.job_id, INSTANCE_A).await;
    // Simulate a keepalive / Ping-Pong refreshing client liveness after the
    // job already entered recovery. The deadline is anchored to
    // recovering_since, not last_seen, so this must not extend it.
    registry.set_last_seen_for_test(CLIENT_ID, now_ts()).await;
    age_recovering_since(&registry, &job.job_id, job_recovery_grace_secs() + 1).await;

    recovery_timeout_sweep(&registry).await;
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().status,
        "lost",
        "keepalive must not extend the recovery deadline"
    );
}

#[tokio::test]
async fn runner_reconnect_before_deadline_cancels_timeout() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, request) = start_and_take_over(&registry, INSTANCE_A).await;
    drive_into_recovering(&registry, &job.job_id, INSTANCE_A).await;
    // Reconnect before the deadline: same instance submits inventory that
    // reconciles the job back to running.
    let reconciled = snapshot_from_request(&job, &request, "running", 2, stream("one\n", 1, false));
    register(
        &registry,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![reconciled],
        },
    )
    .await;
    let running = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(running.status, "running");
    assert_eq!(running.recovery_state.as_deref(), Some("reconciled"));

    // A subsequent sweep is a no-op on the now-running job even if
    // recovering_since were somehow stale.
    recovery_timeout_sweep(&registry).await;
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().status,
        "running"
    );
}

#[tokio::test]
async fn terminal_observed_late_update_after_timeout_is_idempotent() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&registry, INSTANCE_A).await;
    drive_into_recovering(&registry, &job.job_id, INSTANCE_A).await;
    age_recovering_since(&registry, &job.job_id, job_recovery_grace_secs() + 1).await;
    recovery_timeout_sweep(&registry).await;
    let first = registry.get_job(&job.job_id).await.unwrap();
    let (first_terminal_observed_at, first_revision) = {
        let inner = registry.inner.lock().await;
        let record = inner.jobs_by_id.get(&job.job_id).unwrap();
        (
            record.observation.terminal_observed_at.unwrap(),
            record
                .observation.revision
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    };

    // An older-sequence update is dropped by the seq guard.
    let _ = registry
        .update_job(update(INSTANCE_A, &job.job_id, 1, "running", None, false))
        .await;
    // An equal-sequence update is also a no-op.
    let _ = registry
        .update_job(update(INSTANCE_A, &job.job_id, 2, "running", None, false))
        .await;
    // Even a newer-sequence update cannot revive a terminal job.
    let _ = registry
        .update_job(update(
            INSTANCE_A,
            &job.job_id,
            9,
            "running",
            Some("late\n"),
            false,
        ))
        .await;
    let lost = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(lost.status, first.status, "terminal job must not revive");
    assert_eq!(lost.ended_at, first.ended_at, "ended_at unchanged");
    assert_eq!(lost.error, first.error, "terminal error unchanged");
    assert_eq!(
        lost.recovery_reason_code, first.recovery_reason_code,
        "terminal recovery reason unchanged"
    );
    let inner = registry.inner.lock().await;
    let record = inner.jobs_by_id.get(&job.job_id).unwrap();
    assert_eq!(
        record.observation.terminal_observed_at,
        Some(first_terminal_observed_at),
        "late updates must not extend retention"
    );
    assert_eq!(
        record
            .observation.revision
            .load(std::sync::atomic::Ordering::Relaxed),
        first_revision
    );
}

#[tokio::test]
async fn registry_rebuild_re_anchors_deadline_after_restart() {
    // Simulate a server restart: a fresh registry starts empty until the runner
    // reconnects and submits inventory, which re-anchors recovering_since.
    let registry_a = RunnerRegistry::default();
    register(&registry_a, INSTANCE_A, empty_inventory()).await;
    let (job, request) = start_and_take_over(&registry_a, INSTANCE_A).await;
    registry_a
        .update_job(update(
            INSTANCE_A,
            &job.job_id,
            2,
            "running",
            Some("one\n"),
            false,
        ))
        .await
        .unwrap();
    registry_a.reconcile_disconnect(CLIENT_ID, INSTANCE_A).await;

    // "Restart": fresh registry; the recovering job is gone until the runner
    // reconnects and submits a running inventory snapshot.
    let registry_b = RunnerRegistry::default();
    let snapshot = snapshot_from_request(&job, &request, "running", 3, stream("one\n", 1, false));
    register(
        &registry_b,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![snapshot],
        },
    )
    .await;
    let restored = registry_b.get_job(&job.job_id).await.unwrap();
    assert_eq!(restored.status, "running");
    assert_eq!(
        restored.recovery_state.as_deref(),
        Some("reconciled"),
        "fresh window after restart: job is reconciled, not recovering"
    );
    assert!(restored.recovered_after_server_restart);

    // If the runner now disconnects again, a fresh recovery window begins and
    // the sweep's deadline is measured from the new recovering_since.
    registry_b.reconcile_disconnect(CLIENT_ID, INSTANCE_A).await;
    assert_eq!(
        registry_b.get_job(&job.job_id).await.unwrap().status,
        "recovering"
    );
    age_recovering_since(&registry_b, &job.job_id, job_recovery_grace_secs() + 1).await;
    recovery_timeout_sweep(&registry_b).await;
    let lost = registry_b.get_job(&job.job_id).await.unwrap();
    assert_eq!(lost.status, "lost");
    assert_eq!(
        lost.recovery_reason_code.as_deref(),
        Some("runner_recovery_deadline_exceeded")
    );
}

#[tokio::test]
async fn sweep_only_transitions_expired_jobs_and_leaves_recent_recovering() {
    let registry = RunnerRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    // Two jobs: one aged past the deadline, one that just entered recovery.
    let (expired, _) = start_and_take_over(&registry, INSTANCE_A).await;
    let (fresh, _) = start_and_take_over(&registry, INSTANCE_A).await;
    // A single transport disconnect drives both into `recovering`.
    registry.reconcile_disconnect(CLIENT_ID, INSTANCE_A).await;
    assert_eq!(
        registry.get_job(&expired.job_id).await.unwrap().status,
        "recovering"
    );
    assert_eq!(
        registry.get_job(&fresh.job_id).await.unwrap().status,
        "recovering"
    );
    age_recovering_since(&registry, &expired.job_id, job_recovery_grace_secs() + 1).await;
    age_recovering_since(&registry, &fresh.job_id, 1).await;

    recovery_timeout_sweep(&registry).await;
    assert_eq!(
        registry.get_job(&expired.job_id).await.unwrap().status,
        "lost"
    );
    assert_eq!(
        registry.get_job(&fresh.job_id).await.unwrap().status,
        "recovering",
        "non-expired recovering job is left alone by the sweep"
    );
}
