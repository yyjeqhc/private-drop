use super::*;
use crate::mcp_gateway::{
    McpGatewayDispatchState, McpGatewayProvider, McpGatewayRequest, McpGatewayResponse,
    McpGatewayResponsePayload,
};
use crate::runner_protocol::{
    RunnerPolicySummary, RunnerPollRequest, RunnerRegisterRequest, RunnerResultPayload,
    RunnerResultRequest,
};

fn bridge_provider(provider_instance_id: &str) -> McpGatewayProvider {
    McpGatewayProvider {
        provider_id: "provider".to_string(),
        provider_instance_id: provider_instance_id.to_string(),
        name: "Provider".to_string(),
    }
}

async fn register_bridge_runner(registry: &RunnerRegistry) {
    registry
        .register(RunnerRegisterRequest {
            client_id: "bridge-runner".to_string(),
            runner_instance_id: "bridge-instance".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            capabilities: crate::test_support::current_runner_capabilities(
                RunnerCapabilities::default(),
            ),
            host_context: None,
            policy: Some(RunnerPolicySummary {
                mcp_gateway_providers: Some(vec![bridge_provider("provider-instance")]),
                ..Default::default()
            }),
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
        })
        .await
        .unwrap();
}

fn list_request(provider_instance_id: &str) -> McpGatewayRequest {
    McpGatewayRequest::ToolsList {
        provider_id: "provider".to_string(),
        provider_instance_id: provider_instance_id.to_string(),
    }
}

fn bridge_registration(
    client_id: &str,
    runner_instance_id: &str,
    providers: Option<Vec<McpGatewayProvider>>,
) -> RunnerRegisterRequest {
    current_runner_registration(RunnerRegisterRequest {
        client_id: client_id.to_string(),
        runner_instance_id: runner_instance_id.to_string(),
        runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
        display_name: None,
        owner: Some("alice".to_string()),
        hostname: None,
        capabilities: RunnerCapabilities::default(),
        host_context: None,
        policy: providers.map(|providers| RunnerPolicySummary {
            mcp_gateway_providers: Some(providers),
            ..Default::default()
        }),
        process_started_at: None,
        build: None,
        job_concurrency_limit: None,
        job_inventory: None,
        coding_agent_providers: None,
        coding_agent_inventory: None,
    })
}

#[tokio::test]
async fn bridge_registration_inventory_is_bounded_and_exact() {
    let registry = RunnerRegistry::default();
    registry
        .register(bridge_registration(
            "valid-bridge-runner",
            "valid-instance",
            Some(vec![bridge_provider("provider-instance")]),
        ))
        .await
        .unwrap();
    let view = registry
        .get_runner_view("valid-bridge-runner")
        .await
        .unwrap();
    assert_eq!(
        view.policy
            .as_ref()
            .and_then(|policy| policy.mcp_gateway_providers.as_ref())
            .unwrap(),
        &vec![bridge_provider("provider-instance")]
    );

    for (name, providers) in [
        (
            "duplicate-id",
            vec![
                bridge_provider("provider-instance-a"),
                McpGatewayProvider {
                    provider_id: "provider".to_string(),
                    provider_instance_id: "provider-instance-b".to_string(),
                    name: "Provider B".to_string(),
                },
            ],
        ),
        (
            "duplicate-instance",
            vec![
                bridge_provider("provider-instance"),
                McpGatewayProvider {
                    provider_id: "provider-b".to_string(),
                    provider_instance_id: "provider-instance".to_string(),
                    name: "Provider B".to_string(),
                },
            ],
        ),
    ] {
        assert!(registry
            .register(bridge_registration(
                name,
                &format!("{name}-instance"),
                Some(providers),
            ))
            .await
            .unwrap_err()
            .contains("invalid MCP gateway provider inventory"));
    }

    let excessive = (0..=crate::mcp_gateway::MCP_GATEWAY_MAX_PROVIDERS)
        .map(|index| McpGatewayProvider {
            provider_id: format!("provider-{index}"),
            provider_instance_id: format!("instance-{index}"),
            name: format!("Provider {index}"),
        })
        .collect();
    assert!(registry
        .register(bridge_registration(
            "excessive",
            "excessive-instance",
            Some(excessive),
        ))
        .await
        .is_err());
    assert!(registry
        .register(bridge_registration(
            "malformed",
            "malformed-instance",
            Some(vec![McpGatewayProvider {
                provider_id: "Bad Provider".to_string(),
                provider_instance_id: "instance".to_string(),
                name: "Bad".to_string(),
            }]),
        ))
        .await
        .is_err());
    registry
        .register(bridge_registration(
            "no-gateway-inventory",
            "no-gateway-inventory-instance",
            None,
        ))
        .await
        .expect(
            "Runner without gateway inventory remains valid but cannot receive gateway requests",
        );
}

#[tokio::test]
async fn bridge_enqueue_rechecks_owner_and_exact_runner_instance() {
    let registry = RunnerRegistry::default();
    register_bridge_runner(&registry).await;

    let bob = auth_context(Some("bob"), false);
    assert!(registry
        .enqueue_mcp_gateway(
            "bridge-runner",
            "bridge-instance",
            list_request("provider-instance"),
            Some(&bob),
            "bob".to_string(),
        )
        .await
        .unwrap_err()
        .contains("unavailable"));

    let alice = auth_context(Some("alice"), false);
    assert!(registry
        .enqueue_mcp_gateway(
            "bridge-runner",
            "stale-instance",
            list_request("provider-instance"),
            Some(&alice),
            "alice".to_string(),
        )
        .await
        .unwrap_err()
        .contains("stale Runner"));

    assert!(registry
        .enqueue_mcp_gateway(
            "bridge-runner",
            "bridge-instance",
            list_request("stale-provider-instance"),
            Some(&alice),
            "alice".to_string(),
        )
        .await
        .unwrap_err()
        .contains("stale provider"));

    let inner = registry.inner.lock().await;
    assert!(inner.pending_by_id.is_empty());
    assert!(inner.mcp_gateway_waiters.is_empty());
}

#[tokio::test]
async fn bridge_dequeue_rechecks_exact_runner_instance_after_replacement() {
    let registry = RunnerRegistry::default();
    register_bridge_runner(&registry).await;
    let alice = auth_context(Some("alice"), false);
    let (_request_id, receiver) = registry
        .enqueue_mcp_gateway(
            "bridge-runner",
            "bridge-instance",
            list_request("provider-instance"),
            Some(&alice),
            "test".to_string(),
        )
        .await
        .unwrap();

    // Simulate the narrow invariant violation between admission and dequeue.
    // Normal replacement registration already drains synchronous requests, but
    // dequeue itself must carry the exact process fence rather than relying on
    // that separate lifecycle path forever.
    {
        let mut inner = registry.inner.lock().await;
        inner
            .runners
            .get_mut("bridge-runner")
            .unwrap()
            .runner_instance_id = "replacement-instance".to_string();
    }
    let polled = registry
        .poll(RunnerPollRequest {
            client_id: "bridge-runner".to_string(),
            runner_instance_id: "replacement-instance".to_string(),
        })
        .await
        .unwrap();
    assert!(
        polled.is_none(),
        "replacement Runner must not receive stale bridge work"
    );
    let response = receiver.await.unwrap();
    assert_eq!(response.dispatch_state, McpGatewayDispatchState::NotStarted);
    assert_eq!(response.error.as_ref().unwrap().code, "stale_runner");
}

#[tokio::test]
async fn bridge_dequeue_rejects_missing_exact_target_fence() {
    let registry = RunnerRegistry::default();
    register_bridge_runner(&registry).await;
    let alice = auth_context(Some("alice"), false);
    let (request_id, mut receiver) = registry
        .enqueue_mcp_gateway(
            "bridge-runner",
            "bridge-instance",
            list_request("provider-instance"),
            Some(&alice),
            "test".to_string(),
        )
        .await
        .unwrap();

    // Dequeue must fail closed even if admission's exact target fence is lost.
    {
        let mut inner = registry.inner.lock().await;
        let pending = inner.pending_by_id.get_mut(&request_id).unwrap();
        pending.expected_mcp_gateway_runner_instance_id = None;
        pending.expected_mcp_gateway_provider_id = None;
        pending.expected_mcp_gateway_provider_instance_id = None;
    }
    let polled = registry
        .poll(RunnerPollRequest {
            client_id: "bridge-runner".to_string(),
            runner_instance_id: "bridge-instance".to_string(),
        })
        .await
        .unwrap();
    assert!(
        polled.is_none(),
        "unfenced bridge work must not be dispatched"
    );
    let response = receiver.try_recv().unwrap();
    assert_eq!(response.dispatch_state, McpGatewayDispatchState::NotStarted);
    assert_eq!(response.error.as_ref().unwrap().code, "stale_provider");
    let inner = registry.inner.lock().await;
    assert!(!inner.pending_by_id.contains_key(&request_id));
    assert!(!inner.mcp_gateway_waiters.contains_key(&request_id));
}

#[tokio::test]
async fn bridge_dequeue_rechecks_exact_provider_instance_after_inventory_change() {
    let registry = RunnerRegistry::default();
    register_bridge_runner(&registry).await;
    let alice = auth_context(Some("alice"), false);
    let (_request_id, receiver) = registry
        .enqueue_mcp_gateway(
            "bridge-runner",
            "bridge-instance",
            list_request("provider-instance"),
            Some(&alice),
            "test".to_string(),
        )
        .await
        .unwrap();

    {
        let mut inner = registry.inner.lock().await;
        inner
            .runners
            .get_mut("bridge-runner")
            .unwrap()
            .policy
            .as_mut()
            .unwrap()
            .mcp_gateway_providers = Some(vec![bridge_provider("replacement-provider-instance")]);
    }
    let polled = registry
        .poll(RunnerPollRequest {
            client_id: "bridge-runner".to_string(),
            runner_instance_id: "bridge-instance".to_string(),
        })
        .await
        .unwrap();
    assert!(
        polled.is_none(),
        "changed provider lease must not receive stale work"
    );
    let response = receiver.await.unwrap();
    assert_eq!(response.dispatch_state, McpGatewayDispatchState::NotStarted);
    assert_eq!(response.error.as_ref().unwrap().code, "stale_provider");
}

#[tokio::test]
async fn dispatched_bridge_disconnect_is_outcome_unknown_and_not_replayed() {
    let registry = RunnerRegistry::default();
    register_bridge_runner(&registry).await;
    let alice = auth_context(Some("alice"), false);
    let (request_id, receiver) = registry
        .enqueue_mcp_gateway(
            "bridge-runner",
            "bridge-instance",
            McpGatewayRequest::ToolsCall {
                provider_id: "provider".to_string(),
                provider_instance_id: "provider-instance".to_string(),
                name: "effect".to_string(),
                arguments: serde_json::json!({}),
                expected_schema: crate::mcp_gateway::McpGatewaySchemaObservation {
                    input_schema: serde_json::json!({"type": "object"}),
                    output_schema: None,
                    annotations: None,
                },
            },
            Some(&alice),
            "test".to_string(),
        )
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: "bridge-runner".to_string(),
            runner_instance_id: "bridge-instance".to_string(),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request.request_id, request_id);
    assert!(request.mcp_gateway.is_some());

    registry
        .reconcile_disconnect("bridge-runner", "bridge-instance")
        .await;
    let response = receiver.await.unwrap();
    assert_eq!(
        response.dispatch_state,
        McpGatewayDispatchState::OutcomeUnknown
    );
    assert_eq!(response.error.as_ref().unwrap().code, "runner_unavailable");

    let inner = registry.inner.lock().await;
    assert!(inner.pending_by_id.is_empty());
    assert!(inner.mcp_gateway_waiters.is_empty());
    assert!(inner
        .queues_by_runner
        .get("bridge-runner")
        .is_none_or(|queue| queue.is_empty()));
}

#[tokio::test]
async fn typed_bridge_result_is_correlated_once() {
    let registry = RunnerRegistry::default();
    register_bridge_runner(&registry).await;
    let alice = auth_context(Some("alice"), false);
    let (request_id, receiver) = registry
        .enqueue_mcp_gateway(
            "bridge-runner",
            "bridge-instance",
            list_request("provider-instance"),
            Some(&alice),
            "test".to_string(),
        )
        .await
        .unwrap();
    registry
        .poll(RunnerPollRequest {
            client_id: "bridge-runner".to_string(),
            runner_instance_id: "bridge-instance".to_string(),
        })
        .await
        .unwrap()
        .unwrap();
    let payload = RunnerResultPayload {
        result: RunnerResultRequest {
            client_id: "bridge-runner".to_string(),
            runner_instance_id: "bridge-instance".to_string(),
            request_id: request_id.clone(),
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: None,
            error: None,
        },
        command_execution_state: None,
        mcp_gateway: Some(McpGatewayResponse::success(
            McpGatewayResponsePayload::Tools { tools: Vec::new() },
        )),
        plugin_gateway: None,
        coding_agent: None,
    };
    registry.complete(payload.clone()).await.unwrap();
    let response = receiver.await.unwrap();
    assert!(matches!(
        response.payload,
        Some(McpGatewayResponsePayload::Tools { .. })
    ));
    assert!(registry
        .complete(payload)
        .await
        .unwrap_err()
        .contains("unknown"));
}
