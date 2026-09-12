use super::*;

/// Helper: register a long-lived-transport (WebSocket/QUIC) client bound to
/// a server-internal `connection_id`. Mirrors what `runner_ws`/`runner_quic`
/// do at register time. Returns the view along with the connection_id so a
/// test can drive the connection-scoped poll/touch/result/update APIs.
async fn register_with_connection(
    registry: &RunnerRegistry,
    client_id: &str,
    instance: &str,
    connection_id: &str,
) -> RunnerView {
    registry
        .register_streaming_session(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: client_id.to_string(),
                runner_instance_id: instance.to_string(),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: None,
                owner: Some("alice".to_string()),
                hostname: None,
                host_context: None,
                capabilities: async_job_capabilities(),
                policy: None,
            },
            None,
            connection_id,
            RunnerTransport::WebSocket,
            Arc::new(Notify::new()),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn streaming_registration_commits_transport_connection_and_notifier_together() {
    let registry = RunnerRegistry::default();
    let notify = Arc::new(Notify::new());
    let registration = runner_registration("atomic-stream", "atomic-instance", Vec::new());

    let view = registry
        .register_streaming_session(
            registration,
            None,
            "atomic-connection",
            RunnerTransport::WebSocket,
            notify.clone(),
        )
        .await
        .unwrap();
    assert_eq!(view.transport, TRANSPORT_WEBSOCKET);
    assert!(view.projects.is_empty());
    assert_eq!(
        view.project_inventory
            .as_ref()
            .map(|status| status.sync_state.as_str()),
        Some("pending")
    );

    let inner = registry.inner.lock().await;
    let client = inner.runners.get("atomic-stream").unwrap();
    let notifier = inner.notifiers.get("atomic-stream").unwrap();
    assert_eq!(client.connection_id.as_deref(), Some("atomic-connection"));
    assert_eq!(client.transport, RunnerTransport::WebSocket);
    assert_eq!(notifier.runner_instance_id, "atomic-instance");
    assert_eq!(notifier.connection_id.as_deref(), Some("atomic-connection"));
    assert!(Arc::ptr_eq(&notifier.notify, &notify));
}

#[tokio::test]
async fn delayed_polling_offline_cannot_disconnect_same_instance_streaming_reconnect() {
    let registry = RunnerRegistry::default();
    register_with_instance(&registry, "oe", "inst-x").await;

    let streaming = register_with_connection(&registry, "oe", "inst-x", "conn-b").await;
    assert_eq!(streaming.transport, TRANSPORT_WEBSOCKET);
    assert!(streaming.connected);

    assert!(!registry
        .reconcile_polling_disconnect("oe", "inst-x")
        .await
        .unwrap());
    let current = registry.get_runner_view("oe").await.unwrap();
    assert_eq!(current.runner_instance_id, "inst-x");
    assert_eq!(current.transport, TRANSPORT_WEBSOCKET);
    assert!(current.connected);
    assert_eq!(current.disconnected_at, None);
}

#[tokio::test]
async fn streaming_registration_rejects_polling_transport_authority() {
    let registry = RunnerRegistry::default();
    let error = registry
        .register_streaming_session(
            runner_registration("invalid-stream", "inst", Vec::new()),
            None,
            "invalid-connection",
            RunnerTransport::Polling,
            Arc::new(Notify::new()),
        )
        .await
        .unwrap_err();
    assert_eq!(error, "streaming Runner transport is unsupported");
    assert!(registry.list_runners().await.is_empty());
}

#[tokio::test]
async fn failed_streaming_registration_preserves_current_session_exactly() {
    let registry = RunnerRegistry::default();
    let notify_a = Arc::new(Notify::new());
    let initial = runner_registration("atomic-preserve", "atomic-instance", Vec::new());
    let (_view_a, cancel_a) = registry
        .register_streaming_session_with_cancel(
            initial,
            None,
            "connection-a",
            RunnerTransport::WebSocket,
            notify_a.clone(),
        )
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &registry,
        "atomic-preserve",
        "atomic-instance",
        vec![project_summary("original", "/tmp/original")],
    )
    .await;
    let before = {
        let inner = registry.inner.lock().await;
        inner.runners.get("atomic-preserve").unwrap().clone()
    };

    let notify_b = Arc::new(Notify::new());
    let mut rejected = runner_registration("atomic-preserve", "atomic-instance", Vec::new());
    rejected.runner_protocol_generation = RunnerProtocolGenerationNumber::new(3);
    let error = registry
        .register_streaming_session_with_cancel(
            rejected,
            None,
            "connection-b",
            RunnerTransport::WebSocket,
            notify_b.clone(),
        )
        .await
        .unwrap_err();
    assert_eq!(error, "agent_protocol_generation is unsupported");
    assert!(
        !*cancel_a.borrow(),
        "failed replacement must not cancel the authoritative connection"
    );
    assert!(
        registry
            .get_runner_view_for_connection("atomic-preserve", "atomic-instance", "connection-a")
            .await
            .expect("failed replacement must preserve connection A")
            .connected
    );

    let inner = registry.inner.lock().await;
    let after = inner.runners.get("atomic-preserve").unwrap();
    let notifier = inner.notifiers.get("atomic-preserve").unwrap();
    assert_eq!(after.connection_id, before.connection_id);
    assert_eq!(after.transport, before.transport);
    assert_eq!(after.last_seen, before.last_seen);
    assert_eq!(after.registered_at, before.registered_at);
    assert_eq!(after.connected_at, before.connected_at);
    assert_eq!(after.projects.len(), before.projects.len());
    assert_eq!(after.projects[0].id, before.projects[0].id);
    assert_eq!(after.projects[0].path, before.projects[0].path);
    assert_eq!(notifier.connection_id.as_deref(), Some("connection-a"));
    assert!(Arc::ptr_eq(&notifier.notify, &notify_a));
    assert!(!Arc::ptr_eq(&notifier.notify, &notify_b));
}

// ------------------------------------------------------------------------
// Connection-scoped lease: same-instance transport reconnect races.
// A replaced connection (same client_id + same agent_instance_id but a
// newer connection_id) must not let the older socket dequeue new
// requests, refresh liveness, or clobber the new connection's metadata.
// ------------------------------------------------------------------------

#[tokio::test]
async fn stale_connection_poll_cannot_steal_new_request() {
    // Same runner instance registers over connection A, a request is
    // queued, then the instance reconnects over connection B (new lease).
    // Connection A's connection-scoped poll must be rejected with a stale
    // connection error AND leave the request in the queue / undispatched /
    // job un-transitioned (atomic: not just a stale error string). B then
    // polls and is the only one to receive the request.
    let registry = RunnerRegistry::default();
    register_with_connection(&registry, "oe", "inst-x", "conn-a").await;

    // Start an async job (queued -> agent_queued only on dispatch).
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: None,
                command: Some("sleep 1".to_string()),
                timeout_secs: Some(1),
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
    // The job starts queued with one pending request in the queue.
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().status,
        "queued"
    );

    // Same instance reconnects over connection B; B takes the lease.
    register_with_connection(&registry, "oe", "inst-x", "conn-b").await;

    // A's connection-scoped poll is rejected with the stable stale error.
    let err = registry
        .poll_for_connection(
            RunnerPollRequest {
                client_id: "oe".to_string(),
                runner_instance_id: "inst-x".to_string(),
            },
            "conn-a",
        )
        .await
        .unwrap_err();
    assert!(
        err.contains("transport connection is no longer active"),
        "error was: {err}"
    );

    // Atomicity: the request must still be queued, undispatched, and the
    // job must still be queued (no queued -> agent_queued transition).
    let pending_depth = registry
        .get_runner_view("oe")
        .await
        .unwrap()
        .pending_requests;
    assert_eq!(pending_depth, 1, "stale poll must not dequeue the request");
    {
        let inner = registry.inner.lock().await;
        let request_id = inner
            .jobs_by_id
            .get(&job.job_id)
            .and_then(|j| j.request_id.clone());
        let request_id = request_id.expect("job has a request_id");
        let pending = inner
            .pending_by_id
            .get(&request_id)
            .expect("request still pending");
        assert!(
            !pending.dispatched,
            "stale poll must not mark request dispatched"
        );
        assert_eq!(
            inner.jobs_by_id.get(&job.job_id).unwrap().public_status(),
            "queued",
            "stale poll must not transition the job"
        );
    }

    // B's connection-scoped poll receives the request (exactly once).
    let polled_b = registry
        .poll_for_connection(
            RunnerPollRequest {
                client_id: "oe".to_string(),
                runner_instance_id: "inst-x".to_string(),
            },
            "conn-b",
        )
        .await
        .unwrap()
        .expect("current connection must receive the request");
    assert_eq!(polled_b.kind, "start_job");
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().status,
        "agent_queued"
    );
    // The queue is now drained: a second poll by either connection gets None.
    let again_a = registry
        .poll_for_connection(
            RunnerPollRequest {
                client_id: "oe".to_string(),
                runner_instance_id: "inst-x".to_string(),
            },
            "conn-a",
        )
        .await;
    // A is still stale, so this is an error (not a None success).
    assert!(again_a.is_err());
}

#[tokio::test]
async fn stale_connection_keepalive_does_not_refresh_new_lease() {
    // After a same-instance reconnect, a delayed Ping/Pong from the old
    // connection must not refresh the new connection's last_seen or revive
    // a disconnected client. The current connection's keepalive does
    // refresh.
    let registry = RunnerRegistry::default();
    register_with_connection(&registry, "oe", "inst-x", "conn-a").await;
    register_with_connection(&registry, "oe", "inst-x", "conn-b").await;

    // Pin the current client's last_seen to a known stale value so a
    // successful touch would observably advance it.
    let pinned = chrono::Utc::now().timestamp() - 90;
    registry.set_last_seen_for_test("oe", pinned).await;

    // A's connection-scoped touch fails and leaves last_seen unchanged.
    let err = registry
        .touch_runner_for_connection("oe", "inst-x", "conn-a")
        .await
        .unwrap_err();
    assert!(
        err.contains("transport connection is no longer active"),
        "error was: {err}"
    );
    assert_eq!(
        registry.get_runner_view("oe").await.unwrap().last_seen,
        pinned,
        "stale connection touch must not refresh last_seen"
    );

    // B's connection-scoped touch succeeds and advances last_seen.
    registry
        .touch_runner_for_connection("oe", "inst-x", "conn-b")
        .await
        .unwrap();
    assert!(
        registry.get_runner_view("oe").await.unwrap().last_seen > pinned,
        "current connection touch must refresh last_seen"
    );

    // An even newer connection C supersedes B; B's touch now fails too.
    register_with_connection(&registry, "oe", "inst-x", "conn-c").await;
    let err = registry
        .touch_runner_for_connection("oe", "inst-x", "conn-b")
        .await
        .unwrap_err();
    assert!(
        err.contains("transport connection is no longer active"),
        "superseded connection touch must be rejected, error was: {err}"
    );
}

#[tokio::test]
async fn stale_connection_runtime_metadata_does_not_overwrite_current() {
    // A stale same-instance connection must not overwrite the current
    // connection's provider metadata. The current connection can.
    let registry = RunnerRegistry::default();
    let register_with_policy = async |connection_id: &str| {
        registry
            .register_streaming_session(
                RunnerRegisterRequest {
                    process_started_at: None,
                    build: None,
                    job_concurrency_limit: None,
                    job_inventory: None,
                    coding_agent_providers: None,
                    coding_agent_inventory: None,
                    client_id: "oe".to_string(),
                    runner_instance_id: "inst-x".to_string(),
                    runner_protocol_generation:
                        crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                    display_name: None,
                    owner: Some("alice".to_string()),
                    hostname: None,
                    host_context: None,
                    capabilities: async_job_capabilities(),
                    policy: Some(RunnerPolicySummary::default()),
                },
                None,
                connection_id,
                RunnerTransport::WebSocket,
                Arc::new(Notify::new()),
            )
            .await
            .unwrap()
    };
    register_with_policy("conn-a").await;
    register_with_policy("conn-b").await;

    let provider_status = |strategy: &str| ToolProvidersStatus {
        strategy: strategy.to_string(),
        claude_code: ClaudeCodeProviderStatus {
            enabled: true,
            version: None,
            available: true,
            process_state: "running".to_string(),
            discovered_tool_names: Vec::new(),
            capabilities: std::collections::BTreeMap::new(),
            last_error_code: None,
            last_call: None,
        },
        config_reload: Default::default(),
    };

    let mcp_inventory = |instance: &str| {
        vec![webcodex_core::mcp_gateway::McpGatewayProvider {
            provider_id: "blender".to_string(),
            provider_instance_id: instance.to_string(),
            name: "Blender".to_string(),
        }]
    };

    // Current connection B reports both observational provider status and the
    // authoritative MCP routing inventory in one lease-fenced metadata update.
    registry
        .update_runtime_metadata_for_connection(
            "oe",
            "inst-x",
            "conn-b",
            Some(provider_status("claude_code")),
            Some(mcp_inventory("mcp-b")),
        )
        .await
        .unwrap();
    {
        let inner = registry.inner.lock().await;
        let client = inner.runners.get("oe").unwrap();
        assert_eq!(
            client
                .policy
                .as_ref()
                .unwrap()
                .tool_providers
                .as_ref()
                .unwrap()
                .strategy,
            "claude_code"
        );
        assert_eq!(
            client
                .policy
                .as_ref()
                .unwrap()
                .mcp_gateway_providers
                .as_ref()
                .unwrap()[0]
                .provider_instance_id,
            "mcp-b"
        );
    }

    // Stale connection A tries to overwrite both metadata families. The exact
    // connection lease rejects the whole update, including MCP routing authority.
    let err = registry
        .update_runtime_metadata_for_connection(
            "oe",
            "inst-x",
            "conn-a",
            Some(provider_status("native")),
            Some(mcp_inventory("mcp-stale")),
        )
        .await
        .unwrap_err();
    assert!(
        err.contains("transport connection is no longer active"),
        "{err}"
    );
    {
        let inner = registry.inner.lock().await;
        let client = inner.runners.get("oe").unwrap();
        assert_eq!(
            client
                .policy
                .as_ref()
                .unwrap()
                .tool_providers
                .as_ref()
                .unwrap()
                .strategy,
            "claude_code",
            "stale connection must not overwrite current metadata"
        );
        assert_eq!(
            client
                .policy
                .as_ref()
                .unwrap()
                .mcp_gateway_providers
                .as_ref()
                .unwrap()[0]
                .provider_instance_id,
            "mcp-b",
            "stale connection must not overwrite MCP routing authority"
        );
    }

    // The active connection can explicitly clear the MCP inventory. `None`
    // means no update, while `Some([])` is an authoritative empty inventory.
    registry
        .update_runtime_metadata_for_connection("oe", "inst-x", "conn-b", None, Some(Vec::new()))
        .await
        .unwrap();
    let inner = registry.inner.lock().await;
    assert!(inner
        .runners
        .get("oe")
        .unwrap()
        .policy
        .as_ref()
        .unwrap()
        .mcp_gateway_providers
        .as_ref()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn stale_connection_disconnect_cleanup_is_noop_for_current_lease() {
    // Same-instance reconnect: A's delayed disconnect cleanup must not
    // touch B's notifier/queue/liveness. Extends the existing same-instance
    // reconnect coverage to the connection lease.
    let registry = RunnerRegistry::default();
    register_with_connection(&registry, "oe", "inst-x", "conn-a").await;
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

    // B reconnects (same instance) and installs its own notifier.
    register_with_connection(&registry, "oe", "inst-x", "conn-b").await;

    // A's delayed disconnect cleanup is a no-op: B's job is not lost.
    registry
        .reconcile_disconnect_for_connection("oe", "inst-x", "conn-a")
        .await;
    assert_ne!(
        registry.get_job(&job.job_id).await.unwrap().status,
        "lost",
        "stale connection cleanup must not mark current job lost"
    );
    {
        let inner = registry.inner.lock().await;
        let client = inner.runners.get("oe").unwrap();
        let notifier = inner.notifiers.get("oe").unwrap();
        assert_eq!(client.connection_id.as_deref(), Some("conn-b"));
        assert_eq!(client.transport, RunnerTransport::WebSocket);
        assert_eq!(notifier.connection_id.as_deref(), Some("conn-b"));
        assert_eq!(notifier.runner_instance_id, "inst-x");
    }
    // B's notifier survives A's cleanup and B's own dispatch still works.
    let polled = registry
        .poll_for_connection(
            RunnerPollRequest {
                client_id: "oe".to_string(),
                runner_instance_id: "inst-x".to_string(),
            },
            "conn-b",
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(polled.kind, "start_job");

    // B's own disconnect does reconcile the job to lost.
    registry
        .reconcile_disconnect_for_connection("oe", "inst-x", "conn-b")
        .await;
    assert_eq!(registry.get_job(&job.job_id).await.unwrap().status, "lost");
}

#[tokio::test]
async fn late_result_on_stale_connection_is_accepted_without_refreshing_liveness() {
    // A request dispatched to A (same instance) before the reconnect must
    // still complete on a late result arriving over the stale connection
    // A — it belongs to the same instance — but must NOT refresh B's
    // liveness. A cannot then poll a new request that arrived after B's
    // register.
    let registry = RunnerRegistry::default();
    register_with_connection(&registry, "oe", "inst-x", "conn-a").await;

    // Enqueue a sync request and let A poll it (still current lease).
    let (request_id, rx) = registry
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
    let polled_a = registry
        .poll_for_connection(
            RunnerPollRequest {
                client_id: "oe".to_string(),
                runner_instance_id: "inst-x".to_string(),
            },
            "conn-a",
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(polled_a.request_id, request_id);

    // Same instance reconnects over B; B is now the current lease.
    register_with_connection(&registry, "oe", "inst-x", "conn-b").await;
    // Pin B's last_seen to an online-but-observable value. A refresh by a
    // successful connection-scoped operation would advance it to `now`; the
    // stale connection must leave it at the pinned value. Staying inside the
    // 60s online window keeps the later enqueue path valid.
    let pinned = chrono::Utc::now().timestamp() - 30;
    registry.set_last_seen_for_test("oe", pinned).await;

    // The late result arrives over stale connection A. It is accepted
    // (same instance) and resolves the waiter.
    registry
        .complete_for_connection(
            RunnerResultRequest {
                client_id: "oe".to_string(),
                runner_instance_id: "inst-x".to_string(),
                request_id: request_id.clone(),
                exit_code: Some(0),
                stdout: Some("hi".to_string()),
                stderr: None,
                duration_ms: Some(1),
                error: None,
            }
            .into(),
            "conn-a",
        )
        .await
        .unwrap();
    let response = rx.await.unwrap();
    assert!(response.success);
    // But it did NOT refresh B's liveness.
    assert_eq!(
        registry.get_runner_view("oe").await.unwrap().last_seen,
        pinned,
        "late result on stale connection must not refresh new lease liveness"
    );

    // A cannot now poll a request enqueued after B's register. Enqueue a
    // new request under B's lease and verify A's poll is rejected.
    let (_new_request_id, _new_rx) = registry
        .enqueue_run(
            ShellRunRequest {
                client_id: "oe".to_string(),
                cwd: None,
                command: "echo two".to_string(),
                stdin: None,
                timeout_secs: 5,
                wait_timeout_secs: 0,
            },
            "tester".to_string(),
        )
        .await
        .unwrap();
    let err = registry
        .poll_for_connection(
            RunnerPollRequest {
                client_id: "oe".to_string(),
                runner_instance_id: "inst-x".to_string(),
            },
            "conn-a",
        )
        .await
        .unwrap_err();
    assert!(
        err.contains("transport connection is no longer active"),
        "{err}"
    );

    // B receives the new request.
    let polled_b = registry
        .poll_for_connection(
            RunnerPollRequest {
                client_id: "oe".to_string(),
                runner_instance_id: "inst-x".to_string(),
            },
            "conn-b",
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(polled_b.command, "echo two");
}

#[tokio::test]
async fn late_job_update_on_stale_connection_is_accepted_without_refreshing_liveness() {
    // A job dispatched to A before the reconnect: its high-sequence job
    // update arriving over stale connection A is still applied (ownership
    // + update_seq), but does not refresh B's liveness. A replaced runner
    // instance is still rejected.
    let registry = RunnerRegistry::default();
    register_with_connection(&registry, "oe", "inst-x", "conn-a").await;
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
    // A polls/dispatches the job (still current lease).
    registry
        .poll_for_connection(
            RunnerPollRequest {
                client_id: "oe".to_string(),
                runner_instance_id: "inst-x".to_string(),
            },
            "conn-a",
        )
        .await
        .unwrap()
        .unwrap();

    // Same instance reconnects over B.
    register_with_connection(&registry, "oe", "inst-x", "conn-b").await;
    // Pin to an online-but-observable value: a refresh would advance it to
    // `now`, but the stale connection must leave it pinned. Staying online
    // also prevents `get_job`'s status refresh from marking the active job
    // lost while we inspect it.
    let pinned = chrono::Utc::now().timestamp() - 30;
    registry.set_last_seen_for_test("oe", pinned).await;

    // Late job update over stale connection A is accepted and applied.
    registry
        .update_job_for_connection(
            RunnerJobUpdateRequest {
                client_id: "oe".to_string(),
                runner_instance_id: "inst-x".to_string(),
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
            },
            "conn-a",
        )
        .await
        .unwrap();
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().status,
        "running"
    );
    // But B's liveness was not refreshed.
    assert_eq!(
        registry.get_runner_view("oe").await.unwrap().last_seen,
        pinned,
        "late job update on stale connection must not refresh new lease liveness"
    );

    // A replaced runner instance is still rejected outright (a brand new
    // instance cannot submit updates for the old instance's job). Age the
    // old instance out so the replacement can take the lease.
    registry
        .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
        .await;
    register_with_instance(&registry, "oe", "inst-y").await;
    let err = registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst-x".to_string(),
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
        .await
        .unwrap_err();
    assert!(
        err.contains("no longer the active instance"),
        "replaced runner instance must be rejected, error was: {err}"
    );
}
