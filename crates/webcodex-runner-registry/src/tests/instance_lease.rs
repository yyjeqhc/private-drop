use super::*;

#[tokio::test]
async fn lease_first_register_accepts_instance() {
    let registry = RunnerRegistry::default();
    let view = register_with_instance(&registry, "oe", "inst-a").await;
    assert_eq!(view.runner_instance_id, "inst-a");
    assert!(view.connected);
    // The view/list path exposes the instance id.
    let runners = registry.list_runners().await;
    assert_eq!(runners[0].runner_instance_id, "inst-a");
}

#[tokio::test]
async fn lease_same_instance_reregister_accepts() {
    let registry = RunnerRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;
    // Same client_id + same instance id is a reconnect/refresh: accepted.
    let _ = register_with_instance(&registry, "oe", "inst-a").await;
    let view = registry.get_runner_view("oe").await.unwrap();
    assert_eq!(view.runner_instance_id, "inst-a");
    assert!(view.connected);
}

#[tokio::test]
async fn lease_different_online_instance_takes_over_immediately_and_retires_old_instance() {
    let registry = RunnerRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;
    // A deliberate new process registration is stronger evidence than the
    // passive last_seen grace: it takes over immediately even while A is fresh.
    let view = registry
        .register(current_runner_registration(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "oe".to_string(),
            runner_instance_id: "inst-b".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            host_context: None,
            capabilities: async_job_capabilities(),
            policy: None,
        }))
        .await
        .unwrap();
    assert_eq!(view.runner_instance_id, "inst-b");
    assert!(view.connected);

    // Once replaced, A cannot use a fresh registration to steal the lease back.
    let err = registry
        .register(runner_registration("oe", "inst-a", Vec::new()))
        .await
        .unwrap_err();
    assert!(err.contains("instance was replaced and cannot reclaim the lease"));
}

#[tokio::test]
async fn lease_different_instance_takeover_refreshes_stale_liveness() {
    let registry = RunnerRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;
    // Age the first instance past the online window so it reads as stale.
    registry
        .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
        .await;
    // Explicit takeover also replaces a passively stale lease and refreshes liveness.
    let _ = register_with_instance(&registry, "oe", "inst-b").await;
    let view = registry.get_runner_view("oe").await.unwrap();
    assert_eq!(view.runner_instance_id, "inst-b");
    assert!(view.connected);
}

#[tokio::test]
async fn lease_stale_instance_poll_rejected() {
    let registry = RunnerRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;
    // Explicit registration replaces A immediately; A is stale by lease identity,
    // not because a wall-clock liveness timeout elapsed.
    register_with_instance(&registry, "oe", "inst-b").await;

    // The stale instance A can no longer poll.
    let err = registry
        .poll(RunnerPollRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst-a".to_string(),
        })
        .await
        .unwrap_err();
    assert!(
        err.contains("no longer the active instance"),
        "error was: {err}"
    );

    // The active instance B can still poll.
    registry
        .poll(RunnerPollRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst-b".to_string(),
        })
        .await
        .expect("active instance must poll");
}

#[tokio::test]
async fn lease_stale_instance_result_rejected() {
    let registry = RunnerRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;
    // Enqueue a synchronous request and let instance A poll it (dispatched).
    let (request_id, mut rx) = registry
        .enqueue_run(
            ShellRunRequest {
                client_id: "oe".to_string(),
                cwd: None,
                command: "echo hi".to_string(),
                stdin: None,
                timeout_secs: 5,
                wait_timeout_secs: 0,
            },
            "tester".to_string(),
        )
        .await
        .unwrap();
    let _ = registry
        .poll(RunnerPollRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst-a".to_string(),
        })
        .await
        .unwrap()
        .unwrap();

    // Replace instance A with B immediately. The dispatched synchronous request
    // is owned by the replaced Runner process, so it is failed and drained at
    // replacement with `request_dispatched` preserved.
    register_with_instance(&registry, "oe", "inst-b").await;

    // The stale instance A cannot submit the result.
    let err = registry
        .complete(RunnerResultRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst-a".to_string(),
            request_id: request_id.clone(),
            exit_code: Some(0),
            stdout: Some("hi".to_string()),
            stderr: None,
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap_err();
    assert!(
        err.contains("no longer the active instance"),
        "error was: {err}"
    );

    // The waiter resolves with the replacement error and preserves the truth
    // that the request had already been dispatched.
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), &mut rx)
        .await
        .expect("replaced instance waiter must resolve promptly")
        .expect("replaced instance waiter must not be dropped");
    assert!(!response.success);
    assert_eq!(response.request_dispatched, Some(true));

    // The active instance B cannot submit the old instance's result: the
    // request record was drained with the replaced process, so it is no
    // longer present for the new lease.
    let err = registry
        .complete(RunnerResultRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst-b".to_string(),
            request_id,
            exit_code: Some(0),
            stdout: Some("hi".to_string()),
            stderr: None,
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap_err();
    assert!(
        err.contains("unknown or expired shell request"),
        "replacement must not inherit the replaced request: {err}"
    );
}

#[tokio::test]
async fn lease_stale_instance_job_update_rejected() {
    // A new `agent_instance_id` replacing the old instance terminates the
    // old instance's active/recovering jobs to `lost` with
    // `runner_instance_replaced` immediately at registration. The old
    // instance's late update is rejected, the new instance cannot inherit
    // or update the old instance's job, and the terminal state never
    // revives.
    let registry = RunnerRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;
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
            "tester".to_string(),
        )
        .await
        .unwrap();

    // Replace instance A with B immediately. The replacement must terminate
    // A's job to `lost` at registration time.
    register_with_instance(&registry, "oe", "inst-b").await;

    let lost = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(lost.status, "lost");
    assert_eq!(
        lost.recovery_reason_code.as_deref(),
        Some("runner_instance_replaced")
    );
    assert!(lost.ended_at.is_some(), "replaced job must record ended_at");
    assert_eq!(
        lost.recovery_state.as_deref(),
        Some("lost_after_reconcile"),
        "replaced job must record lost_after_reconcile"
    );

    // The stale instance A cannot update the job (lease check).
    let err = registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst-a".to_string(),
            update_seq: None,
            job_id: job.job_id.clone(),
            request_id: None,
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
        .unwrap_err();
    assert!(
        err.contains("no longer the active instance"),
        "error was: {err}"
    );

    // The active instance B cannot inherit or update A's job: it belongs
    // to the replaced runner instance.
    let err = registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst-b".to_string(),
            update_seq: None,
            job_id: job.job_id.clone(),
            request_id: None,
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
        .unwrap_err();
    assert!(
        err.contains("replaced runner instance"),
        "active instance must not inherit replaced job: {err}"
    );

    // The terminal state is stable: a second late update from A does not
    // revive the job or change the first `ended_at` / reason.
    let first_ended_at = lost.ended_at.unwrap();
    let _ = registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst-a".to_string(),
            update_seq: None,
            job_id: job.job_id.clone(),
            request_id: None,
            status: "completed".to_string(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: None,
            stderr_tail: None,
            log_snapshot: None,
            exit_code: Some(0),
            duration_ms: Some(1),
            error: None,
            command_execution_state: None,
            validation_progress: None,
            activity: None,
            finished: true,
        })
        .await;
    let still_lost = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(still_lost.status, "lost");
    assert_eq!(still_lost.ended_at, Some(first_ended_at));
    assert_eq!(
        still_lost.recovery_reason_code.as_deref(),
        Some("runner_instance_replaced")
    );
}

#[tokio::test]
async fn lease_reconcile_disconnect_stale_instance_is_noop() {
    // A delayed disconnect from a stale, replaced instance must not affect
    // the current active instance: it must not clear B's notifier, not mark
    // B's freshly-created job lost/recovering, and not change A's old job
    // which was already terminated to `lost` (`runner_instance_replaced`)
    // at replacement time. Only B's own disconnect reconciles B's job.
    let registry = RunnerRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;
    // Install a notifier for instance A.
    let notify_a = Arc::new(Notify::new());
    registry
        .register_notifier("oe", "inst-a", notify_a.clone())
        .await
        .unwrap();
    // Start a job under instance A. It is terminated to `lost` when B
    // replaces A, before any disconnect runs.
    let old_job = registry
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
            "tester".to_string(),
        )
        .await
        .unwrap();

    // Age out A and let B take over. The replacement terminates A's job.
    registry
        .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
        .await;
    register_with_instance(&registry, "oe", "inst-b").await;
    // B installs its own notifier.
    let notify_b = Arc::new(Notify::new());
    registry
        .register_notifier("oe", "inst-b", notify_b.clone())
        .await
        .unwrap();

    // B starts a fresh job of its own after the replacement.
    let b_job = registry
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
            "tester".to_string(),
        )
        .await
        .unwrap();

    // Snapshot A's old job terminal state before the stale disconnect.
    let old_lost = registry.get_job(&old_job.job_id).await.unwrap();
    assert_eq!(old_lost.status, "lost");
    assert_eq!(
        old_lost.recovery_reason_code.as_deref(),
        Some("runner_instance_replaced")
    );
    let old_ended_at = old_lost.ended_at.unwrap();

    // A's transport finally disconnects. This must be a no-op: B stays the
    // current instance, B's notifier stays installed, B's job stays
    // active, and A's old job keeps its first `ended_at`/reason.
    registry.reconcile_disconnect("oe", "inst-a").await;

    let view = registry.get_runner_view("oe").await.unwrap();
    assert_eq!(view.runner_instance_id, "inst-b");
    assert!(view.connected, "stale disconnect must not drop B's lease");

    // B's notifier remains installed (still addressable) and B's job is
    // untouched.
    let b_view = registry.get_job(&b_job.job_id).await.unwrap();
    assert_ne!(
        b_view.status, "lost",
        "stale disconnect must not mark B's active job lost"
    );
    assert_ne!(
        b_view.status, "recovering",
        "stale disconnect must not drive B's job into recovering"
    );

    let old_after = registry.get_job(&old_job.job_id).await.unwrap();
    assert_eq!(old_after.status, "lost");
    assert_eq!(old_after.ended_at, Some(old_ended_at));
    assert_eq!(
        old_after.recovery_reason_code.as_deref(),
        Some("runner_instance_replaced")
    );

    // B can still poll/update/complete its own job after A's stale
    // disconnect.
    let updated = registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst-b".to_string(),
            update_seq: None,
            job_id: b_job.job_id.clone(),
            request_id: None,
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
        .expect("B must still update its own job after A's stale disconnect");
    assert_eq!(updated.status, "running");

    // Only B's own disconnect reconciles B's job. A non-reconciliation
    // client's active job becomes `lost` without reconciliation support.
    registry.reconcile_disconnect("oe", "inst-b").await;
    let b_final = registry.get_job(&b_job.job_id).await.unwrap();
    assert_eq!(b_final.status, "lost");
    assert_eq!(
        b_final.recovery_reason_code.as_deref(),
        Some("runner_disconnected_without_reconciliation")
    );
    // A's old job is unaffected by B's disconnect.
    let old_final = registry.get_job(&old_job.job_id).await.unwrap();
    assert_eq!(old_final.status, "lost");
    assert_eq!(old_final.ended_at, Some(old_ended_at));
}

#[tokio::test]
async fn lease_register_notifier_rejects_stale_instance() {
    let registry = RunnerRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;
    // Replace A with B.
    registry
        .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
        .await;
    register_with_instance(&registry, "oe", "inst-b").await;
    // A's late notifier registration must be rejected so it cannot
    // overwrite B's notifier.
    let err = registry
        .register_notifier("oe", "inst-a", Arc::new(Notify::new()))
        .await
        .unwrap_err();
    assert!(
        err.contains("no longer the active instance"),
        "error was: {err}"
    );
    // B can still install its notifier.
    registry
        .register_notifier("oe", "inst-b", Arc::new(Notify::new()))
        .await
        .expect("active instance must install notifier");
}

#[tokio::test]
async fn lease_register_rejects_empty_instance_id() {
    let registry = RunnerRegistry::default();
    let err = registry
        .register(current_runner_registration(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "oe".to_string(),
            runner_instance_id: "".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: None,
            hostname: None,
            host_context: None,
            capabilities: crate::test_support::current_runner_capabilities(
                RunnerCapabilities::default(),
            ),
            policy: None,
        }))
        .await
        .unwrap_err();
    assert!(err.contains("agent_instance_id"), "error was: {err}");
}

#[tokio::test]
async fn lease_replacement_transfers_exact_detached_inventory_to_new_instance() {
    let registry = RunnerRegistry::default();
    let capabilities = || RunnerCapabilities {
        jobs: true,
        async_jobs: true,
        async_shell_jobs: true,
        job_state_reconciliation: true,
        coding_agent_runs: false,
        structured_process_argv: true,
        structured_execution_jobs: true,
        detached_process_jobs: true,
        ..Default::default()
    };
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
            runner_instance_id: "inst-a".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            host_context: None,
            capabilities: capabilities(),
            policy: None,
        }))
        .await
        .unwrap();

    let job = registry
        .start_job_with_metadata(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: Some("/tmp".to_string()),
                command: Some(String::new()),
                timeout_secs: Some(60),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "tester".to_string(),
            ShellJobStartMetadata {
                purpose: Some("test".to_string()),
                shell: Some("direct_argv".to_string()),
                structured_execution: Some(StructuredJobExecution::DetachedProcess(
                    crate::runner_protocol::ShellProcessArgv {
                        executable: "/bin/sleep".to_string(),
                        args: vec!["60".to_string()],
                    },
                )),
                detached_idempotency_key: Some("lease-detached-transfer".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst-a".to_string(),
        })
        .await
        .unwrap()
        .expect("detached start must dispatch to original instance");
    assert_eq!(request.kind, "start_detached_process_job");
    assert_eq!(request.job_id.as_deref(), Some(job.job_id.as_str()));

    registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst-a".to_string(),
            job_id: job.job_id.clone(),
            request_id: job.request_id.clone(),
            update_seq: Some(1),
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

    let snapshot = {
        let inner = registry.inner.lock().await;
        let record = inner.jobs_by_id.get(&job.job_id).unwrap();
        crate::runner_protocol::ShellJobSnapshot {
            job_id: record.job_id.clone(),
            request_id: record.request_id.clone().unwrap(),
            status: record.public_status().to_string(),
            update_seq: record.last_update_seq,
            created_at: record.created_at,
            started_at: record.started_at,
            ended_at: record.ended_at,
            exit_code: record.exit_code,
            duration_ms: record.duration_ms,
            error: record.error.clone(),
            command_execution_state: record.command_execution_state,
            context: crate::runner_protocol::ShellJobContext {
                runtime_project_id: record.project_id.clone(),
                workflow_session_id: record.session_id.clone(),
                ssh_resource: record.ssh_resource.clone(),
                project_cwd: record.project_cwd.clone(),
                cwd: record.cwd.clone(),
                purpose: record.purpose.clone(),
                shell: record.shell.clone(),
                command_preview: record.command_preview.clone(),
                validation_steps: record.validation_steps.clone(),
                validation: record.validation.clone(),
                structured_execution: record.structured_execution.clone(),
            },
            stdout: Default::default(),
            stderr: Default::default(),
            validation_progress: record.validation_progress.clone(),
            activity: record.activity,
        }
    };

    registry
        .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
        .await;
    let view = registry
        .register(current_runner_registration(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: Some(crate::runner_protocol::ShellJobInventory {
                active_complete: true,
                jobs: vec![snapshot.clone()],
            }),
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "oe".to_string(),
            runner_instance_id: "inst-b".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            host_context: None,
            capabilities: capabilities(),
            policy: None,
        }))
        .await
        .expect("exact detached recovery inventory must transfer to replacement instance");
    assert_eq!(view.runner_instance_id, "inst-b");

    {
        let inner = registry.inner.lock().await;
        let record = inner.jobs_by_id.get(&job.job_id).unwrap();
        assert_eq!(record.runner_instance_id, "inst-b");
        assert_eq!(record.public_status(), "running");
        assert_eq!(record.last_update_seq, snapshot.update_seq);
        assert_eq!(
            record.recovery.public_reason(),
            Some("detached_instance_transfer")
        );
    }

    let updated = registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst-b".to_string(),
            job_id: job.job_id.clone(),
            request_id: job.request_id.clone(),
            update_seq: Some(snapshot.update_seq + 1),
            status: "running".to_string(),
            stdout_chunk: Some("continued\n".to_string()),
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
        .expect("replacement instance must own subsequent detached updates");
    assert_eq!(updated.status, "running");

    let stale = registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst-a".to_string(),
            job_id: job.job_id.clone(),
            request_id: job.request_id.clone(),
            update_seq: Some(snapshot.update_seq + 2),
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
        .unwrap_err();
    assert!(
        stale.contains("no longer the active instance")
            || stale.contains("replaced runner instance"),
        "stale instance must remain fenced: {stale}"
    );
}
