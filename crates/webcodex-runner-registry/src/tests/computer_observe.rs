use super::*;

#[tokio::test]
async fn computer_enqueue_requires_exact_owner_and_distinct_capability() {
    let registry = RunnerRegistry::default();
    register_computer_test_runner(
        &registry,
        "computer-owned",
        "alice",
        false,
        false,
        false,
        false,
    )
    .await;
    let alice = auth_context(Some("alice"), false);
    let error = registry
        .enqueue_computer(
            "computer-owned".to_string(),
            "computer_list_windows",
            r#"{"limit":1}"#.to_string(),
            "alice".to_string(),
            Some(&alice),
            5,
        )
        .await
        .unwrap_err();
    assert!(error.contains("does not support computer_observe"));

    register_computer_test_runner(
        &registry,
        "computer-owned",
        "alice",
        true,
        false,
        false,
        false,
    )
    .await;
    let bob = auth_context(Some("bob"), false);
    let error = registry
        .enqueue_computer(
            "computer-owned".to_string(),
            "computer_list_windows",
            r#"{"limit":1}"#.to_string(),
            "bob".to_string(),
            Some(&bob),
            5,
        )
        .await
        .unwrap_err();
    assert!(error.contains("unknown shell client"), "{error}");
    assert!(!error.contains("owned by"), "{error}");

    let (_request_id, _rx) = registry
        .enqueue_computer(
            "computer-owned".to_string(),
            "computer_snapshot",
            r#"{"surface_id":"surface_test"}"#.to_string(),
            "alice".to_string(),
            Some(&alice),
            5,
        )
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: "computer-owned".to_string(),
            runner_instance_id: "computer-inst".to_string(),
        })
        .await
        .unwrap()
        .expect("queued computer request");
    assert_eq!(request.kind, "computer_snapshot");
    assert!(request.command.is_empty());
    assert!(request.process.is_none());
    assert!(request.script.is_none());
}

#[tokio::test]
async fn computer_snapshot_region_requires_additive_capability() {
    let registry = RunnerRegistry::default();
    let alice = auth_context(Some("alice"), false);
    register_computer_test_runner(
        &registry,
        "computer-region-old",
        "alice",
        true,
        false,
        false,
        false,
    )
    .await;
    let payload = r#"{"surface_id":"surface_test","region":{"x":0,"y":0,"width":10,"height":10},"max_width":null,"max_height":null}"#;
    let error = registry
        .enqueue_computer(
            "computer-region-old".to_string(),
            "computer_snapshot_region",
            payload.to_string(),
            "alice".to_string(),
            Some(&alice),
            5,
        )
        .await
        .unwrap_err();
    assert!(error.contains("does not support computer_snapshot_region"));

    registry
        .register(current_runner_registration(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "computer-region-only".to_string(),
            runner_instance_id: "computer-inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            host_context: None,
            capabilities: RunnerCapabilities {
                shell: true,
                file_read: true,
                computer_observe: false,
                computer_snapshot_region: true,
                ..Default::default()
            },
            policy: None,
        }))
        .await
        .unwrap();
    let error = registry
        .enqueue_computer(
            "computer-region-only".to_string(),
            "computer_snapshot_region",
            payload.to_string(),
            "alice".to_string(),
            Some(&alice),
            5,
        )
        .await
        .unwrap_err();
    assert!(error.contains("does not support computer_observe"));

    registry
        .register(current_runner_registration(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "computer-region-new".to_string(),
            runner_instance_id: "computer-inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            host_context: None,
            capabilities: RunnerCapabilities {
                shell: true,
                file_read: true,
                computer_observe: true,
                computer_snapshot_region: true,
                ..Default::default()
            },
            policy: None,
        }))
        .await
        .unwrap();
    let (_request_id, _rx) = registry
        .enqueue_computer(
            "computer-region-new".to_string(),
            "computer_snapshot_region",
            payload.to_string(),
            "alice".to_string(),
            Some(&alice),
            5,
        )
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: "computer-region-new".to_string(),
            runner_instance_id: "computer-inst".to_string(),
        })
        .await
        .unwrap()
        .expect("queued region snapshot request");
    assert_eq!(request.kind, "computer_snapshot_region");
    assert_eq!(request.stdin.as_deref(), Some(payload));
    assert!(request.command.is_empty());
}

#[tokio::test]
async fn computer_snapshot_display_preserves_large_native_image_response_stdout() {
    let registry = RunnerRegistry::default();
    let alice = auth_context(Some("alice"), false);
    registry
        .register(current_runner_registration(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "computer-display-large".to_string(),
            runner_instance_id: "display-large-inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            host_context: None,
            capabilities: RunnerCapabilities {
                computer_display_observe: true,
                ..Default::default()
            },
            policy: None,
        }))
        .await
        .unwrap();

    let payload = r#"{"display_id":"display_0123456789abcdef0123456789abcdef","max_width":null,"max_height":null}"#;
    let (request_id, response_rx) = registry
        .enqueue_computer(
            "computer-display-large".to_string(),
            "computer_snapshot_display",
            payload.to_string(),
            "alice".to_string(),
            Some(&alice),
            5,
        )
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: "computer-display-large".to_string(),
            runner_instance_id: "display-large-inst".to_string(),
        })
        .await
        .unwrap()
        .expect("queued display snapshot request");
    assert_eq!(request.kind, "computer_snapshot_display");

    let stdout = "x".repeat(super::super::ORDINARY_RESULT_STREAM_RETENTION_BYTES + 1024);
    assert!(stdout.len() < crate::artifact_policy::MAX_MCP_IMAGE_RESPONSE_BYTES);
    registry
        .complete(RunnerResultRequest {
            client_id: "computer-display-large".to_string(),
            runner_instance_id: "display-large-inst".to_string(),
            request_id,
            exit_code: Some(0),
            stdout: Some(stdout.clone()),
            stderr: None,
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap();

    let response = response_rx.await.unwrap();
    assert!(response.success);
    assert_eq!(response.stdout.as_deref(), Some(stdout.as_str()));
}
