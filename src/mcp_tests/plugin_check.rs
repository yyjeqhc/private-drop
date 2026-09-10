use super::*;
use crate::runner_protocol::RunnerResultPayload;
use std::sync::Arc;
use webcodex_core::plugin::{
    PluginCheckDiagnostic, PluginCheckPhase, PluginCheckReport, PluginCheckToolSummary,
    PluginGatewayRequest, PluginGatewayResponse, PluginGatewayResponsePayload,
};

fn plugin_auth(include_scope: bool) -> crate::auth::AuthContext {
    let mut auth = mcp_export_api_auth("plugin-check-test-pat", "alice");
    if include_scope {
        auth.scopes.extend([
            crate::auth::SCOPE_PLUGIN_INSPECT.to_string(),
            crate::auth::SCOPE_PLUGIN_INVOKE.to_string(),
            crate::auth::SCOPE_PLUGIN_MANAGE.to_string(),
        ]);
    }
    auth
}

async fn register_plugin_runner(runtime: &ToolRuntime, client_id: &str, runner_instance_id: &str) {
    let mut capabilities = RunnerCapabilities::default();
    capabilities.native_tool_plugins = true;
    runtime
        .runner_registry
        .register(crate::test_support::current_runner_registration(
            RunnerRegisterRequest {
                client_id: client_id.to_string(),
                runner_instance_id: runner_instance_id.to_string(),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: Some(format!("Plugin {client_id}")),
                owner: Some("alice".to_string()),
                hostname: None,
                host_context: None,
                capabilities,
                policy: Some(Default::default()),
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
            },
        ))
        .await
        .unwrap();
}

async fn wait_for_plugin_request(
    runtime: &ToolRuntime,
    client_id: &str,
    runner_instance_id: &str,
) -> crate::runner_protocol::RunnerRequest {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(request) = runtime
            .runner_registry
            .poll(RunnerPollRequest {
                client_id: client_id.to_string(),
                runner_instance_id: runner_instance_id.to_string(),
            })
            .await
            .unwrap()
        {
            return request;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Plugin check did not dispatch"
        );
        tokio::task::yield_now().await;
    }
}

async fn complete_plugin_request(
    runtime: &ToolRuntime,
    request: crate::runner_protocol::RunnerRequest,
    runner_instance_id: &str,
    response: PluginGatewayResponse,
) {
    runtime
        .runner_registry
        .complete(RunnerResultPayload {
            result: RunnerResultRequest {
                client_id: request.client_id,
                runner_instance_id: runner_instance_id.to_string(),
                request_id: request.request_id,
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: None,
                error: None,
            },
            command_execution_state: None,
            mcp_gateway: None,
            plugin_gateway: Some(response),
            coding_agent: None,
        })
        .await
        .unwrap();
}

fn check_call(runner: &str, extra: Value) -> JsonRpcRequest {
    let mut arguments = json!({
        "action": "check",
        "runner": runner,
        "plugin": "repo-tools"
    });
    if let Some(extra) = extra.as_object() {
        arguments.as_object_mut().unwrap().extend(extra.clone());
    }
    rpc(
        "tools/call",
        Some(json!(760)),
        json!({
            "name": crate::plugin_gateway::PLUGIN_TOOL_NAME,
            "arguments": arguments
        }),
    )
}

async fn tools_list(runtime: &ToolRuntime, auth: &crate::auth::AuthContext) -> Value {
    match handle_mcp_request(
        runtime,
        rpc("tools/list", Some(json!(761)), json!({})),
        Some(auth),
    )
    .await
    {
        McpOutcome::Ok(value) => value,
        other => panic!("tools/list failed: {other:?}"),
    }
}

#[tokio::test]
async fn plugin_check_tool_spec_and_argument_contract_fail_closed_before_dispatch() {
    let spec = registered_tool_specs()
        .into_iter()
        .find(|spec| spec.name == crate::plugin_gateway::PLUGIN_TOOL_NAME)
        .expect("canonical plugin_tool spec");
    let spec = serde_json::to_value(spec).unwrap();
    assert!(spec["inputSchema"]["properties"]["action"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .any(|action| action == "check"));
    let description = spec["description"].as_str().unwrap();
    assert!(description.contains("Provider tools are never outer WebCodex MCP tools"));
    assert!(description.contains("call accepts only binding + arguments"));

    let runtime = test_runtime();
    let no_scope = handle_mcp_request(
        &runtime,
        check_call("runner-a", json!({})),
        Some(&plugin_auth(false)),
    )
    .await;
    match no_scope {
        McpOutcome::Forbidden { required_scope, .. } => {
            assert_eq!(required_scope, Some(crate::auth::SCOPE_PLUGIN_MANAGE));
        }
        other => panic!("check without plugin:manage must be forbidden: {other:?}"),
    }

    let auth = plugin_auth(true);
    for extra in [
        json!({"tool":"search_symbol"}),
        json!({"binding":"wc_pbind_0123456789abcdef0123456789abcdef"}),
        json!({"arguments":{}}),
    ] {
        let outcome =
            handle_mcp_request(&runtime, check_call("runner-a", extra), Some(&auth)).await;
        let McpOutcome::BadRequest(value) = outcome else {
            panic!(
                "invalid check arguments must fail at the canonical ToolCall parser: {outcome:?}"
            );
        };
        assert_eq!(value["error"]["code"], -32602);
    }

    let missing_runner = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(762)),
            json!({
                "name": crate::plugin_gateway::PLUGIN_TOOL_NAME,
                "arguments": {"action":"check","plugin":"repo-tools"}
            }),
        ),
        Some(&auth),
    )
    .await;
    let McpOutcome::BadRequest(missing_runner) = missing_runner else {
        panic!("missing runner must fail at the canonical ToolCall parser");
    };
    assert_eq!(missing_runner["error"]["code"], -32602);

    let missing_plugin = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(763)),
            json!({
                "name": crate::plugin_gateway::PLUGIN_TOOL_NAME,
                "arguments": {"action":"check","runner":"runner-a"}
            }),
        ),
        Some(&auth),
    )
    .await;
    let McpOutcome::BadRequest(missing_plugin) = missing_plugin else {
        panic!("missing plugin must fail at the canonical ToolCall parser");
    };
    assert_eq!(missing_plugin["error"]["code"], -32602);
    assert!(runtime
        .runner_registry
        .poll(RunnerPollRequest {
            client_id: "runner-a".to_string(),
            runner_instance_id: "runner-instance-a".to_string(),
        })
        .await
        .is_err());
}

#[tokio::test]
async fn plugin_check_routes_exact_runner_and_never_changes_outer_mcp_inventory() {
    let runtime = Arc::new(test_runtime());
    let auth = plugin_auth(true);
    register_plugin_runner(&runtime, "runner-a", "runner-instance-a").await;
    register_plugin_runner(&runtime, "runner-b", "runner-instance-b").await;
    let before = tools_list(&runtime, &auth).await;
    assert!(!before["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "search_symbol"));
    assert!(before["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == crate::plugin_gateway::PLUGIN_TOOL_NAME));

    let task_runtime = Arc::clone(&runtime);
    let task_auth = auth.clone();
    let task = tokio::spawn(async move {
        handle_mcp_request(
            &task_runtime,
            check_call("runner-a", json!({})),
            Some(&task_auth),
        )
        .await
    });

    let request = wait_for_plugin_request(&runtime, "runner-a", "runner-instance-a").await;
    assert!(matches!(
        request.plugin_gateway,
        Some(PluginGatewayRequest::Check { ref provider_id }) if provider_id == "repo-tools"
    ));
    assert!(runtime
        .runner_registry
        .poll(RunnerPollRequest {
            client_id: "runner-b".to_string(),
            runner_instance_id: "runner-instance-b".to_string(),
        })
        .await
        .unwrap()
        .is_none());
    complete_plugin_request(
        &runtime,
        request,
        "runner-instance-a",
        PluginGatewayResponse::success(PluginGatewayResponsePayload::Checked {
            report: PluginCheckReport {
                provider_id: "repo-tools".to_string(),
                ready: true,
                phase: PluginCheckPhase::Ready,
                code: None,
                detail: None,
                tool_count: 1,
                tools: vec![PluginCheckToolSummary {
                    name: "search_symbol".to_string(),
                    title: Some("Search Symbol".to_string()),
                }],
                diagnostic: None,
            },
        }),
    )
    .await;

    let McpOutcome::Ok(value) = task.await.unwrap() else {
        panic!("plugin check failed");
    };
    assert_eq!(value["result"]["isError"], false);
    assert_eq!(
        value["result"]["content"][0]["text"],
        "Plugin metadata available in structuredContent."
    );
    let report = &value["result"]["structuredContent"];
    assert_eq!(report["runner"], "runner-a");
    assert_eq!(report["plugin"], "repo-tools");
    assert_eq!(report["ready"], true);
    assert_eq!(report["phase"], "ready");
    assert_eq!(report["toolCount"], 1);
    assert_eq!(report["tools"][0]["name"], "search_symbol");
    assert!(report.get("startupToolShape").is_none());
    assert!(report.get("binding").is_none());
    let encoded = serde_json::to_string(report).unwrap();
    for forbidden in [
        "runner-instance-a",
        "provider_instance_id",
        "runner_instance_id",
        "command",
        "argv",
        "cwd",
        "env",
        "stderr",
        "pid",
    ] {
        assert!(!encoded.contains(forbidden), "check leaked {forbidden}");
    }

    let after = tools_list(&runtime, &auth).await;
    assert!(!after["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "search_symbol"));
    assert!(after["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == crate::plugin_gateway::PLUGIN_TOOL_NAME));
}

#[tokio::test]
async fn broken_plugin_candidate_is_a_successful_check_diagnostic_result() {
    let runtime = Arc::new(test_runtime());
    let auth = plugin_auth(true);
    register_plugin_runner(&runtime, "runner-a", "runner-instance-a").await;
    let task_runtime = Arc::clone(&runtime);
    let task_auth = auth.clone();
    let task = tokio::spawn(async move {
        handle_mcp_request(
            &task_runtime,
            check_call("runner-a", json!({})),
            Some(&task_auth),
        )
        .await
    });
    let request = wait_for_plugin_request(&runtime, "runner-a", "runner-instance-a").await;
    complete_plugin_request(
        &runtime,
        request,
        "runner-instance-a",
        PluginGatewayResponse::success(PluginGatewayResponsePayload::Checked {
            report: PluginCheckReport {
                provider_id: "repo-tools".to_string(),
                ready: false,
                phase: PluginCheckPhase::Validation,
                code: Some("plugin_tools_list_invalid".to_string()),
                detail: Some(
                    "Plugin tools/list result violates Tool schema or Plugin bounds".to_string(),
                ),
                tool_count: 0,
                tools: vec![],
                diagnostic: Some(PluginCheckDiagnostic {
                    code: "duplicate_tool_name".to_string(),
                    tool: Some("search_symbol".to_string()),
                    field: Some("name".to_string()),
                }),
            },
        }),
    )
    .await;
    let McpOutcome::Ok(value) = task.await.unwrap() else {
        panic!("broken candidate check should still complete as a diagnostic result");
    };
    assert_eq!(value["result"]["isError"], false);
    assert_eq!(value["result"]["structuredContent"]["ready"], false);
    assert_eq!(
        value["result"]["structuredContent"]["code"],
        "plugin_tools_list_invalid"
    );
    assert_eq!(value["result"]["structuredContent"]["phase"], "validation");
    assert_eq!(
        value["result"]["structuredContent"]["diagnostic"]["code"],
        "duplicate_tool_name"
    );
    assert_eq!(
        value["result"]["structuredContent"]["diagnostic"]["tool"],
        "search_symbol"
    );
    assert_eq!(
        value["result"]["structuredContent"]["diagnostic"]["field"],
        "name"
    );
    assert!(value["result"]["structuredContent"]
        .get("binding")
        .is_none());
    let encoded = serde_json::to_string(&value["result"]["structuredContent"]).unwrap();
    for forbidden in [
        "command",
        "argv",
        "cwd",
        "env",
        "stderr",
        "PID",
        "runner_instance_id",
        "provider_instance_id",
        "\"properties\"",
        "\"type\":\"object\"",
    ] {
        assert!(!encoded.contains(forbidden), "check leaked {forbidden}");
    }
}
