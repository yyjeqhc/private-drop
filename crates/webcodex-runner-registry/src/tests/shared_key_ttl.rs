use super::*;

#[tokio::test]
async fn shared_key_offline_ttl_prunes_only_expired_clients_and_all_associated_state() {
    let ttl_secs = 10;
    let registry = RunnerRegistry::with_shared_key_limits_for_test(1, 4, ttl_secs);
    let connected_auth = shared_key_access("ttl-connected");
    let fresh_auth = shared_key_access("ttl-fresh");
    let expired_auth = shared_key_access("ttl-expired");

    registry
        .register_streaming_session(
            runner_registration("ttl-connected", "ttl-connected-instance", Vec::new()),
            Some(&connected_auth),
            "ttl-connected-connection",
            RunnerTransport::WebSocket,
            Arc::new(Notify::new()),
        )
        .await
        .unwrap();
    registry
        .set_last_seen_for_test("ttl-connected", now_ts() - ttl_secs - 100)
        .await;

    registry
        .register_streaming_session(
            runner_registration("ttl-fresh", "ttl-fresh-instance", Vec::new()),
            Some(&fresh_auth),
            "ttl-fresh-connection",
            RunnerTransport::WebSocket,
            Arc::new(Notify::new()),
        )
        .await
        .unwrap();
    registry
        .reconcile_disconnect_for_connection(
            "ttl-fresh",
            "ttl-fresh-instance",
            "ttl-fresh-connection",
        )
        .await;

    registry
        .register_with_auth(
            runner_registration(
                "ttl-expired",
                "ttl-expired-instance",
                vec![project_summary("expired-project", "/tmp/expired")],
            ),
            Some(&expired_auth),
        )
        .await
        .unwrap();
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("ttl-expired".to_string()),
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
    let (sync_request_id, sync_rx) = registry
        .enqueue_run(
            ShellRunRequest {
                client_id: "ttl-expired".to_string(),
                cwd: None,
                command: "echo pending".to_string(),
                stdin: None,
                timeout_secs: 30,
                wait_timeout_secs: 30,
            },
            "test".to_string(),
        )
        .await
        .unwrap();
    registry.record_hidden_cleanup_intent(job.job_id.clone(), None);
    let job_revision = {
        let mut inner = registry.inner.lock().await;
        let expired_at = now_ts() - RUNNER_ONLINE_WINDOW_SECS - ttl_secs - 1;
        let client = inner.runners.get_mut("ttl-expired").unwrap();
        client.last_seen = expired_at;
        client.disconnected_at = Some(expired_at);
        inner.retired_instances.insert(
            "ttl-expired".to_string(),
            std::collections::VecDeque::from(["old-expired-instance".to_string()]),
        );
        inner
            .unregistering_projects
            .insert("agent:ttl-expired:expired-project".to_string(), 1);
        inner
            .jobs_by_id
            .get(&job.job_id)
            .unwrap()
            .observation
            .revision
            .clone()
    };
    let revision_before = job_revision.load(std::sync::atomic::Ordering::Relaxed);
    let observation_token = job.observation_token.clone().expect("observation token");
    let waiter_registry = registry.clone();
    let waiter_auth = expired_auth.clone();
    let waiter_job_id = job.job_id.clone();
    let waiter = tokio::spawn(async move {
        waiter_registry
            .job_log_for_auth(
                Some(&waiter_auth),
                &waiter_job_id,
                None,
                None,
                None,
                Some(&observation_token),
                Some(5),
            )
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let visible_expired = registry.list_runners_for_auth(Some(&expired_auth)).await;
    assert!(visible_expired.is_empty());
    assert!(
        job_revision.load(std::sync::atomic::Ordering::Relaxed) > revision_before,
        "a non-terminal job must transition through the existing lost notifier"
    );
    let (waited_job, _, _, _, _, wait) =
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("lost transition should wake the observation waiter")
            .expect("waiter task should complete")
            .expect("same shared key should still observe the retained Job");
    assert!(wait.changed);
    assert!(wait.terminal);
    assert_eq!(waited_job.status, "lost");
    assert_eq!(
        waited_job.recovery_reason_code.as_deref(),
        Some("shared_key_runner_expired")
    );
    assert!(waited_job
        .error
        .as_deref()
        .is_some_and(|error| error.contains("registration expired")));
    let sync_response = tokio::time::timeout(std::time::Duration::from_secs(1), sync_rx)
        .await
        .expect("expired request waiter should be resolved")
        .expect("expired request waiter should receive a response");
    assert_eq!(sync_response.request_id, sync_request_id);
    assert!(sync_response
        .error
        .as_deref()
        .is_some_and(|error| error.contains("registration expired")));
    let retained = registry
        .get_job_for_auth(Some(&expired_auth), &job.job_id)
        .await
        .expect("same shared key should query the retained lost Job");
    assert_eq!(retained.status, "lost");
    let (first_terminal_observed_at, first_ended_at, first_error, first_reason, first_revision) = {
        let inner = registry.inner.lock().await;
        let record = inner.jobs_by_id.get(&job.job_id).unwrap();
        (
            record
                .observation
                .terminal_observed_at
                .expect("TTL prune records Server terminal observation time"),
            record.ended_at,
            record.error.clone(),
            record.recovery.reason,
            record
                .observation
                .revision
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    };
    registry.prune_expired_shared_key_runners().await;
    {
        let inner = registry.inner.lock().await;
        let record = inner.jobs_by_id.get(&job.job_id).unwrap();
        assert_eq!(record.public_status(), "lost");
        assert_eq!(record.ended_at, first_ended_at);
        assert_eq!(record.error, first_error);
        assert_eq!(record.recovery.reason, first_reason);
        assert_eq!(
            record.observation.terminal_observed_at,
            Some(first_terminal_observed_at),
            "repeated TTL prune must not extend retention"
        );
        assert_eq!(
            record
                .observation
                .revision
                .load(std::sync::atomic::Ordering::Relaxed),
            first_revision,
            "repeated TTL prune must not publish a duplicate terminal update"
        );
    }
    let other_key = shared_key_access("ttl-other-key");
    assert!(registry
        .get_job_for_auth(Some(&other_key), &job.job_id)
        .await
        .unwrap_err()
        .contains("unknown shell job"));
    let managed_reader = auth_context(Some("managed-reader"), false);
    assert!(registry
        .get_job_for_auth(Some(&managed_reader), &job.job_id)
        .await
        .unwrap_err()
        .contains("unknown shell job"));
    assert!(!registry.has_hidden_cleanup_intent_for_test(&job.job_id));
    assert!(registry.list_runner_projects("ttl-expired").await.is_err());
    {
        let inner = registry.inner.lock().await;
        assert!(!inner.runners.contains_key("ttl-expired"));
        assert!(!inner.queues_by_runner.contains_key("ttl-expired"));
        assert!(!inner.notifiers.contains_key("ttl-expired"));
        assert!(!inner.retired_instances.contains_key("ttl-expired"));
        assert!(inner
            .pending_by_id
            .values()
            .all(|pending| pending.request.client_id != "ttl-expired"));
        let retained = inner
            .jobs_by_id
            .get(&job.job_id)
            .expect("lost Job retained");
        assert_eq!(retained.public_status(), "lost");
        assert_eq!(retained.client_id, "ttl-expired");
        assert!(inner
            .unregistering_projects
            .keys()
            .all(|project_id| !project_id.starts_with("agent:ttl-expired:")));
    }

    assert!(registry
        .get_runner_view_for_auth("ttl-connected", Some(&connected_auth))
        .await
        .is_some());
    assert!(registry
        .get_runner_view_for_auth("ttl-fresh", Some(&fresh_auth))
        .await
        .is_some());

    let managed = agent_auth_context("managed", "ttl-managed", vec!["agent:register"]);
    let mut managed_registration =
        runner_registration("ttl-managed", "ttl-managed-instance", Vec::new());
    managed_registration.owner = Some("managed".to_string());
    registry
        .register_with_auth(managed_registration, Some(&managed))
        .await
        .unwrap();
    {
        let mut inner = registry.inner.lock().await;
        let managed_record = inner.runners.get_mut("ttl-managed").unwrap();
        managed_record.last_seen = now_ts() - ttl_secs - 100;
        managed_record.disconnected_at = Some(now_ts() - ttl_secs - 100);
    }
    assert!(registry
        .get_runner_view_for_auth("ttl-managed", Some(&managed))
        .await
        .is_some());

    {
        let mut inner = registry.inner.lock().await;
        inner
            .jobs_by_id
            .get_mut(&job.job_id)
            .unwrap()
            .observation
            .terminal_observed_at =
            Some(now_ts() - crate::runner_protocol::JOB_TERMINAL_RETENTION_SECS);
    }
    crate::recovery_timeout_sweep(&registry).await;
    assert!(registry
        .get_job_for_auth(Some(&expired_auth), &job.job_id)
        .await
        .is_err());
    {
        let inner = registry.inner.lock().await;
        assert!(!inner.request_to_job.values().any(|id| id == &job.job_id));
        assert!(inner
            .pending_by_id
            .values()
            .all(|pending| { pending.job_id.as_deref() != Some(job.job_id.as_str()) }));
        assert!(inner.persistent_waiters.is_empty());
        assert!(inner
            .queues_by_runner
            .values()
            .all(|queue| queue.iter().all(|request_id| {
                inner
                    .pending_by_id
                    .get(request_id)
                    .is_none_or(|pending| pending.job_id.as_deref() != Some(job.job_id.as_str()))
            })));
    }

    registry
        .register_with_auth(
            runner_registration(
                "ttl-expired-replacement",
                "ttl-expired-replacement-instance",
                Vec::new(),
            ),
            Some(&expired_auth),
        )
        .await
        .expect("pruning an expired runner must release its group capacity");
}
