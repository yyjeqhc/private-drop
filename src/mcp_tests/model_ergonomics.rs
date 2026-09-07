use super::*;

// Durable model-ergonomics and MCP tool-surface measurement integration tests.
// Keep these separate from the general HTTP transport lifecycle coverage.

// The asserted outputSchema presence is the default (non-compact) product
// behavior: `WEBCODEX_MCP_COMPACT_SCHEMAS` must stay unset (and serialized
// against other env-mutating tests) for the whole HTTP request.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn http_mcp_tools_list_success() {
    // Default (non-compact) HTTP tools/list: full schema fields present.
    // Compact-mode shape is covered by mcp_tools_list_compact_*.
    let mut env = crate::test_support::TestEnvGuard::new();
    env.remove("WEBCODEX_MCP_COMPACT_SCHEMAS");
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime());
    let service = Service::new(build_test_router(config, db.clone(), runtime));
    let mut resp = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .add_header("x-action-session-id", "tools-list-audit", true)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["id"], 2);
    assert!(body["result"]["tools"].is_array());
    let tools = body["result"]["tools"].as_array().unwrap();
    assert!(!tools.is_empty());
    for tool in tools {
        assert!(tool["name"].is_string());
        assert!(tool["description"].is_string());
        assert!(tool["inputSchema"].is_object());
        if tool["name"].as_str() == Some(crate::mcp_gateway::MCP_TOOL_NAME) {
            assert!(
                tool.get("outputSchema").is_none(),
                "provider-defined MCP gateway must not claim a fixed structuredContent schema"
            );
        } else {
            assert!(
                tool["outputSchema"].is_object(),
                "default HTTP tools/list must include outputSchema for {}",
                tool["name"]
            );
        }
    }
    let event = db.list_action_events("tools-list-audit", 10).unwrap();
    assert_eq!(event.len(), 1);
    assert_eq!(event[0].endpoint, "/mcp");
    assert_eq!(event[0].action_name, "toolsList");
    assert_eq!(event[0].operation.as_deref(), Some("mcp_tools_list"));
    assert_eq!(event[0].status, "success");
    let summary: Value = serde_json::from_str(&event[0].summary_json).unwrap();
    let surface = &summary["tool_surface"];
    assert_eq!(summary["transport"], "mcp");
    assert_eq!(surface["schema_version"], 1);
    assert_eq!(surface["protocol_era"], "legacy");
    assert_eq!(
        surface["runtime_exposure"],
        crate::model_surface::MODEL_SURFACE_LOCAL_CODING
    );
    assert_eq!(surface["compact_schemas"], false);
    assert_eq!(surface["tool_count"].as_u64().unwrap(), tools.len() as u64);
    assert_eq!(
        surface["serialized_tools_bytes"].as_u64().unwrap(),
        serde_json::to_vec(&body["result"]["tools"]).unwrap().len() as u64
    );
    assert_eq!(
        surface["serialized_result_bytes"].as_u64().unwrap(),
        serde_json::to_vec(&body["result"]).unwrap().len() as u64
    );
    assert_eq!(
        surface["gateway_tool_included"],
        tools
            .iter()
            .any(|tool| tool["name"] == crate::mcp_gateway::MCP_TOOL_NAME)
    );
    let durable = serde_json::to_string(&summary).unwrap();
    for forbidden in ["\"tools\"", "inputSchema", "outputSchema", "description"] {
        assert!(
            !durable.contains(forbidden),
            "tools/list audit leaked schema content: {durable}"
        );
    }
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn http_mcp_tools_list_stateless_audit_measures_final_compact_result_and_skips_notifications()
{
    let mut env = crate::test_support::TestEnvGuard::new();
    env.set("WEBCODEX_MCP_COMPACT_SCHEMAS", "1");
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::LocalCoding));
    let service = Service::new(build_test_router(config, db.clone(), runtime));

    let mut response = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "tools/list", true)
        .add_header("x-action-session-id", "tools-list-stateless", true)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2026,
            "method": "tools/list",
            "params": mcp_2026_params(json!({}))
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&response), StatusCode::OK);
    let body: Value = response.take_json().await.unwrap();
    assert_eq!(body["result"]["resultType"], "complete");
    assert!(body["result"].get("ttlMs").is_some());
    assert!(body["result"].get("_meta").is_some());
    let tools = body["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().all(|tool| tool.get("outputSchema").is_none()));

    let events = db.list_action_events("tools-list-stateless", 10).unwrap();
    assert_eq!(events.len(), 1);
    let summary: Value = serde_json::from_str(&events[0].summary_json).unwrap();
    let surface = &summary["tool_surface"];
    assert_eq!(surface["protocol_era"], "stateless_2026");
    assert_eq!(
        surface["runtime_exposure"],
        crate::model_surface::MODEL_SURFACE_LOCAL_CODING
    );
    assert_eq!(surface["compact_schemas"], true);
    assert_eq!(surface["tool_count"].as_u64().unwrap(), tools.len() as u64);
    assert_eq!(
        surface["serialized_tools_bytes"].as_u64().unwrap(),
        serde_json::to_vec(&body["result"]["tools"]).unwrap().len() as u64
    );
    assert_eq!(
        surface["serialized_result_bytes"].as_u64().unwrap(),
        serde_json::to_vec(&body["result"]).unwrap().len() as u64
    );

    let response = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "tools/list", true)
        .add_header("x-action-session-id", "tools-list-notification", true)
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "tools/list",
            "params": mcp_2026_params(json!({}))
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&response), StatusCode::ACCEPTED);
    assert!(db
        .list_action_events("tools-list-notification", 10)
        .unwrap()
        .is_empty());
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn http_mcp_wrong_route_then_corrected_call_is_queryable_without_new_telemetry_schema() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::AdaptiveRuntime));
    let service = Service::new(build_test_router(config, db.clone(), runtime));

    let mut wrong = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "tools/call", true)
        .add_header(
            MCP_NAME_HEADER,
            crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
            true,
        )
        .add_header("x-action-session-id", "wrong-route-recovery", true)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 301,
            "method": "tools/call",
            "params": mcp_2026_params(json!({
                "name": crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
                "arguments": {"tool": "runtime_status", "arguments": {"summary_only": true}}
            }))
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&wrong), StatusCode::OK);
    let wrong_body: Value = wrong.take_json().await.unwrap();
    assert_eq!(wrong_body["result"]["structuredContent"]["success"], false);
    assert_eq!(
        wrong_body["result"]["structuredContent"]["output"]["error_kind"],
        "wrong_invocation_route"
    );

    let mut corrected = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "tools/call", true)
        .add_header(MCP_NAME_HEADER, "runtime_status", true)
        .add_header("x-action-session-id", "wrong-route-recovery", true)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 302,
            "method": "tools/call",
            "params": mcp_2026_params(json!({
                "name": "runtime_status",
                "arguments": {"summary_only": true}
            }))
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&corrected), StatusCode::OK);
    let corrected_body: Value = corrected.take_json().await.unwrap();
    assert_eq!(
        corrected_body["result"]["structuredContent"]["success"],
        true
    );

    let events = db.list_action_events("wrong-route-recovery", 10).unwrap();
    assert_eq!(events.len(), 2);
    let telemetry = events
        .iter()
        .filter_map(|event| serde_json::from_str::<Value>(&event.summary_json).ok())
        .filter_map(|summary| summary.get("model_ergonomics").cloned())
        .collect::<Vec<_>>();
    assert_eq!(telemetry.len(), 2);
    let wrong = telemetry
        .iter()
        .find(|record| record["error_kind"] == "wrong_invocation_route")
        .expect("wrong-route telemetry");
    assert_eq!(wrong["tool_name"], "runtime_status");
    assert_eq!(wrong["success"], false);
    assert_eq!(wrong["execution_state"], "not_started");
    assert!(wrong["serialized_result_bytes"].as_u64().is_some());
    let corrected = telemetry
        .iter()
        .find(|record| record["success"] == true)
        .expect("corrected success telemetry");
    assert_eq!(corrected["tool_name"], "runtime_status");
    assert!(corrected["error_kind"].is_null());
}

#[tokio::test]
async fn http_mcp_tools_list_audit_sink_failure_is_non_blocking() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::LocalCoding));
    let service = Service::new(build_test_router(config, db.clone(), runtime));
    db.conn_for_tests()
        .execute("DROP TABLE action_events", [])
        .unwrap();

    let mut response = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 77,
            "method": "tools/list",
            "params": {}
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&response), StatusCode::OK);
    let body: Value = response.take_json().await.unwrap();
    assert!(body["result"]["tools"].is_array());
}
