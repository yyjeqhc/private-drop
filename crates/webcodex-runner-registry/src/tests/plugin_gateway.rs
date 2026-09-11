use super::*;
use crate::runner_protocol::{
    RunnerPollRequest, RunnerRegisterRequest, RunnerResultPayload, RunnerResultRequest,
};
use serde_json::json;
use webcodex_core::plugin::{
    PluginCheckPhase, PluginCheckReport, PluginDispatchState, PluginGatewayRequest,
    PluginGatewayResponse, PluginGatewayResponsePayload, PluginSchemaObservation, PluginTool,
};

fn plugin_tool() -> PluginTool {
    PluginTool {
        name: "echo".to_string(),
        title: None,
        description: Some("Echo a value".to_string()),
        input_schema: json!({
            "type": "object",
            "properties": {"value": {"type": "string"}}
        }),
        output_schema: None,
        annotations: None,
    }
}

fn plugin_registration(client_id: &str, runner_instance_id: &str) -> RunnerRegisterRequest {
    let mut capabilities = RunnerCapabilities::default();
    capabilities.native_tool_plugins = true;
    current_runner_registration(RunnerRegisterRequest {
        client_id: client_id.to_string(),
        runner_instance_id: runner_instance_id.to_string(),
        runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
        display_name: None,
        owner: Some("alice".to_string()),
        hostname: None,
        capabilities,
        host_context: None,
        policy: Some(Default::default()),
        process_started_at: None,
        build: None,
        job_concurrency_limit: None,
        job_inventory: None,
        coding_agent_providers: None,
        coding_agent_inventory: None,
    })
}

async fn register_plugin_runner(registry: &RunnerRegistry) {
    registry
        .register(plugin_registration("plugin-runner", "runner-instance"))
        .await
        .unwrap();
}

fn provider_list(provider_instance_id: &str) -> PluginGatewayRequest {
    PluginGatewayRequest::ToolsList {
        provider_id: "repo-tools".to_string(),
        provider_instance_id: provider_instance_id.to_string(),
    }
}

#[tokio::test]
async fn plugin_registration_needs_only_native_plugin_capability_not_provider_inventory() {
    let registry = RunnerRegistry::default();
    registry
        .register(plugin_registration("valid-plugin-runner", "valid-instance"))
        .await
        .unwrap();
    let view = registry
        .get_runner_view("valid-plugin-runner")
        .await
        .unwrap();
    assert!(view.capabilities.native_tool_plugins);

    registry
        .register(plugin_registration("valid-plugin-runner", "valid-instance"))
        .await
        .unwrap();
}

#[tokio::test]
async fn typed_operation_decode_does_not_grant_runner_capability() {
    let operation = PluginGatewayRequest::Reload;
    let wire = webcodex_core::runner_protocol::RunnerRequest::from_operation(
        webcodex_core::runner_operation::RunnerInvocationMetadata {
            request_id: "typed-operation-capability-proof".to_string(),
            client_id: "no-plugin-runner".to_string(),
            requested_by: "test".to_string(),
            created_at: 0,
        },
        webcodex_core::runner_operation::RunnerOperation::PluginGateway(operation.clone()),
    )
    .unwrap();
    assert!(matches!(
        wire.decode_operation().unwrap(),
        webcodex_core::runner_operation::RunnerOperation::PluginGateway(_)
    ));

    let registry = RunnerRegistry::default();
    let mut registration = plugin_registration("no-plugin-runner", "no-plugin-instance");
    registration.capabilities.native_tool_plugins = false;
    registry.register(registration).await.unwrap();
    let alice = auth_context(Some("alice"), false);
    let error = registry
        .enqueue_plugin_gateway(
            "no-plugin-runner",
            "no-plugin-instance",
            operation,
            Some(&alice),
            "test".to_string(),
        )
        .await
        .unwrap_err();
    assert_eq!(error, "exact Runner does not support native Tool Plugins");
}

#[tokio::test]
async fn plugin_reload_can_target_exact_plugin_capable_runner_without_registration_catalog() {
    let registry = RunnerRegistry::default();
    registry
        .register(plugin_registration("empty-plugin-runner", "empty-instance"))
        .await
        .unwrap();
    let alice = auth_context(Some("alice"), false);
    let (_request_id, receiver) = registry
        .enqueue_plugin_gateway(
            "empty-plugin-runner",
            "empty-instance",
            PluginGatewayRequest::Reload,
            Some(&alice),
            "test".to_string(),
        )
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: "empty-plugin-runner".to_string(),
            runner_instance_id: "empty-instance".to_string(),
        })
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        request.plugin_gateway,
        Some(PluginGatewayRequest::Reload)
    ));
    registry
        .complete(RunnerResultPayload {
            result: RunnerResultRequest {
                client_id: "empty-plugin-runner".to_string(),
                runner_instance_id: "empty-instance".to_string(),
                request_id: request.request_id,
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: None,
                error: None,
            },
            command_execution_state: None,
            mcp_gateway: None,
            plugin_gateway: Some(PluginGatewayResponse::success(
                PluginGatewayResponsePayload::Reloaded {
                    providers: vec![],
                    failures: vec![],
                },
            )),
            coding_agent: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        receiver.await.unwrap().payload,
        Some(PluginGatewayResponsePayload::Reloaded { .. })
    ));
}

#[tokio::test]
async fn plugin_check_targets_exact_runner_without_registration_provider_inventory() {
    let registry = RunnerRegistry::default();
    registry
        .register(plugin_registration("check-plugin-runner", "check-instance"))
        .await
        .unwrap();
    let alice = auth_context(Some("alice"), false);
    let (_request_id, receiver) = registry
        .enqueue_plugin_gateway(
            "check-plugin-runner",
            "check-instance",
            PluginGatewayRequest::Check {
                provider_id: "repo-tools".to_string(),
            },
            Some(&alice),
            "test".to_string(),
        )
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: "check-plugin-runner".to_string(),
            runner_instance_id: "check-instance".to_string(),
        })
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        request.plugin_gateway,
        Some(PluginGatewayRequest::Check { ref provider_id }) if provider_id == "repo-tools"
    ));
    registry
        .complete(RunnerResultPayload {
            result: RunnerResultRequest {
                client_id: "check-plugin-runner".to_string(),
                runner_instance_id: "check-instance".to_string(),
                request_id: request.request_id,
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: None,
                error: None,
            },
            command_execution_state: None,
            mcp_gateway: None,
            plugin_gateway: Some(PluginGatewayResponse::success(
                PluginGatewayResponsePayload::Checked {
                    report: PluginCheckReport {
                        provider_id: "repo-tools".to_string(),
                        ready: false,
                        phase: PluginCheckPhase::Config,
                        code: Some("plugin_not_configured".to_string()),
                        detail: Some(
                            "requested Plugin provider is not configured in current runner.toml"
                                .to_string(),
                        ),
                        tool_count: 0,
                        tools: vec![],
                        diagnostic: None,
                    },
                },
            )),
            coding_agent: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        receiver.await.unwrap().payload,
        Some(PluginGatewayResponsePayload::Checked { .. })
    ));
}

#[tokio::test]
async fn plugin_check_rejects_mismatched_provider_report_after_dispatch() {
    let registry = RunnerRegistry::default();
    registry
        .register(plugin_registration("check-plugin-runner", "check-instance"))
        .await
        .unwrap();
    let alice = auth_context(Some("alice"), false);
    let (_request_id, receiver) = registry
        .enqueue_plugin_gateway(
            "check-plugin-runner",
            "check-instance",
            PluginGatewayRequest::Check {
                provider_id: "repo-tools".to_string(),
            },
            Some(&alice),
            "test".to_string(),
        )
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: "check-plugin-runner".to_string(),
            runner_instance_id: "check-instance".to_string(),
        })
        .await
        .unwrap()
        .unwrap();
    registry
        .complete(RunnerResultPayload {
            result: RunnerResultRequest {
                client_id: "check-plugin-runner".to_string(),
                runner_instance_id: "check-instance".to_string(),
                request_id: request.request_id,
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: None,
                error: None,
            },
            command_execution_state: None,
            mcp_gateway: None,
            plugin_gateway: Some(PluginGatewayResponse::success(
                PluginGatewayResponsePayload::Checked {
                    report: PluginCheckReport {
                        provider_id: "other-tools".to_string(),
                        ready: false,
                        phase: PluginCheckPhase::Config,
                        code: Some("plugin_not_configured".to_string()),
                        detail: Some(
                            "requested Plugin provider is not configured in current runner.toml"
                                .to_string(),
                        ),
                        tool_count: 0,
                        tools: vec![],
                        diagnostic: None,
                    },
                },
            )),
            coding_agent: None,
        })
        .await
        .unwrap();
    let response = receiver.await.unwrap();
    assert_eq!(response.dispatch_state, PluginDispatchState::OutcomeUnknown);
    assert_eq!(
        response.error.as_ref().map(|error| error.code.as_str()),
        Some("invalid_runner_response")
    );
    assert!(response.payload.is_none());
}

#[tokio::test]
async fn project_catalog_rejects_mixed_runner_response_kind() {
    let registry = RunnerRegistry::default();
    register_plugin_runner(&registry).await;
    let alice = auth_context(Some("alice"), false);
    let (_request_id, receiver) = registry
        .enqueue_plugin_gateway(
            "plugin-runner",
            "runner-instance",
            PluginGatewayRequest::ProjectCatalog {
                project_id: "repo".to_string(),
            },
            Some(&alice),
            "test".to_string(),
        )
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: "plugin-runner".to_string(),
            runner_instance_id: "runner-instance".to_string(),
        })
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        request.plugin_gateway,
        Some(PluginGatewayRequest::ProjectCatalog { ref project_id }) if project_id == "repo"
    ));
    registry
        .complete(RunnerResultPayload {
            result: RunnerResultRequest {
                client_id: "plugin-runner".to_string(),
                runner_instance_id: "runner-instance".to_string(),
                request_id: request.request_id,
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: None,
                error: None,
            },
            command_execution_state: None,
            mcp_gateway: None,
            plugin_gateway: Some(PluginGatewayResponse::success(
                PluginGatewayResponsePayload::Providers { providers: vec![] },
            )),
            coding_agent: None,
        })
        .await
        .unwrap();
    let response = receiver.await.unwrap();
    assert_eq!(response.dispatch_state, PluginDispatchState::OutcomeUnknown);
    assert_eq!(
        response.error.as_ref().map(|error| error.code.as_str()),
        Some("invalid_runner_response")
    );
    assert!(response.payload.is_none());
}

#[tokio::test]
async fn plugin_enqueue_rechecks_owner_and_exact_runner_but_not_provider_inventory() {
    let registry = RunnerRegistry::default();
    register_plugin_runner(&registry).await;
    let bob = auth_context(Some("bob"), false);
    assert!(registry
        .enqueue_plugin_gateway(
            "plugin-runner",
            "runner-instance",
            provider_list("runner-owned-provider-instance"),
            Some(&bob),
            "bob".to_string(),
        )
        .await
        .unwrap_err()
        .contains("unavailable"));

    let alice = auth_context(Some("alice"), false);
    assert!(registry
        .enqueue_plugin_gateway(
            "plugin-runner",
            "stale-runner-instance",
            provider_list("runner-owned-provider-instance"),
            Some(&alice),
            "alice".to_string(),
        )
        .await
        .unwrap_err()
        .contains("stale Runner"));
    let (_request_id, receiver) = registry
        .enqueue_plugin_gateway(
            "plugin-runner",
            "runner-instance",
            provider_list("runner-owned-provider-instance"),
            Some(&alice),
            "alice".to_string(),
        )
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: "plugin-runner".to_string(),
            runner_instance_id: "runner-instance".to_string(),
        })
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        request.plugin_gateway,
        Some(PluginGatewayRequest::ToolsList {
            ref provider_instance_id,
            ..
        }) if provider_instance_id == "runner-owned-provider-instance"
    ));
    registry
        .complete(RunnerResultPayload {
            result: RunnerResultRequest {
                client_id: "plugin-runner".to_string(),
                runner_instance_id: "runner-instance".to_string(),
                request_id: request.request_id,
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: None,
                error: None,
            },
            command_execution_state: None,
            mcp_gateway: None,
            plugin_gateway: Some(PluginGatewayResponse::error(
                PluginDispatchState::NotStarted,
                "stale_plugin_provider",
                "provider instance is not current",
            )),
            coding_agent: None,
        })
        .await
        .unwrap();
    let response = receiver.await.unwrap();
    assert_eq!(response.dispatch_state, PluginDispatchState::NotStarted);
    assert_eq!(
        response.error.as_ref().unwrap().code,
        "stale_plugin_provider"
    );
}

#[tokio::test]
async fn plugin_dequeue_rechecks_exact_runner_before_dispatch() {
    let registry = RunnerRegistry::default();
    register_plugin_runner(&registry).await;
    let alice = auth_context(Some("alice"), false);
    let (_request_id, receiver) = registry
        .enqueue_plugin_gateway(
            "plugin-runner",
            "runner-instance",
            provider_list("provider-instance"),
            Some(&alice),
            "test".to_string(),
        )
        .await
        .unwrap();
    {
        let mut inner = registry.inner.lock().await;
        inner
            .runners
            .get_mut("plugin-runner")
            .unwrap()
            .runner_instance_id = "replacement-runner-instance".to_string();
    }
    assert!(registry
        .poll(RunnerPollRequest {
            client_id: "plugin-runner".to_string(),
            runner_instance_id: "replacement-runner-instance".to_string(),
        })
        .await
        .unwrap()
        .is_none());
    let response = receiver.await.unwrap();
    assert_eq!(response.dispatch_state, PluginDispatchState::NotStarted);
    assert_eq!(response.error.as_ref().unwrap().code, "stale_runner");
}

#[tokio::test]
async fn provider_identity_is_runner_owned_and_forwarded_exactly() {
    let registry = RunnerRegistry::default();
    register_plugin_runner(&registry).await;
    let alice = auth_context(Some("alice"), false);
    let schema = PluginSchemaObservation {
        input_schema: plugin_tool().input_schema,
        output_schema: None,
        annotations: None,
    };
    let (_request_id, receiver) = registry
        .enqueue_plugin_gateway(
            "plugin-runner",
            "runner-instance",
            PluginGatewayRequest::ToolsCall {
                provider_id: "repo-tools".to_string(),
                provider_instance_id: "dynamic-provider-instance".to_string(),
                name: "echo".to_string(),
                arguments: json!({"value":"dynamic"}),
                expected_schema: schema,
            },
            Some(&alice),
            "test".to_string(),
        )
        .await
        .unwrap();
    let request = registry
        .poll(RunnerPollRequest {
            client_id: "plugin-runner".to_string(),
            runner_instance_id: "runner-instance".to_string(),
        })
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        request.plugin_gateway,
        Some(PluginGatewayRequest::ToolsCall {
            ref provider_instance_id,
            ..
        }) if provider_instance_id == "dynamic-provider-instance"
    ));
    registry
        .reconcile_disconnect("plugin-runner", "runner-instance")
        .await;
    let response = receiver.await.unwrap();
    assert_eq!(response.dispatch_state, PluginDispatchState::OutcomeUnknown);
    assert_eq!(response.error.as_ref().unwrap().code, "runner_unavailable");
}
