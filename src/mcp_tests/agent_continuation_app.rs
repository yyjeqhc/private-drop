use super::*;
use std::sync::Arc;

const APP_TOOLS: [&str; 6] = [
    "agent_continuation_bind",
    "agent_continuation_state",
    "agent_continuation_wake_acquire",
    "agent_continuation_wake_prepare",
    "agent_continuation_wake_finish",
    "agent_continuation_unbind",
];

fn tool<'a>(payload: &'a Value, name: &str) -> Option<&'a Value> {
    payload["tools"]
        .as_array()?
        .iter()
        .find(|tool| tool["name"] == name)
}

fn continuation_auth(username: &str) -> crate::auth::AuthContext {
    let mut auth = crate::auth::AuthContext::new(crate::auth::AuthKind::ApiToken);
    auth.user_id = Some(format!("user-{username}"));
    auth.username = Some(username.to_string());
    auth.api_key_id = Some(format!("key-{username}"));
    auth.role = Some("user".to_string());
    auth.scopes = vec![
        crate::auth::SCOPE_RUNTIME_READ.to_string(),
        crate::auth::SCOPE_COMMUNICATION_READ.to_string(),
        crate::auth::SCOPE_COMMUNICATION_MANAGE.to_string(),
    ];
    auth.token_kind = Some("user".to_string());
    auth
}

fn continuation_runtime(
    surface: ModelSurface,
) -> (tempfile::TempDir, Arc<crate::db::Database>, ToolRuntime) {
    let temp = tempfile::tempdir().unwrap();
    let db =
        Arc::new(crate::db::Database::open(&temp.path().join("agent-continuation.db")).unwrap());
    let runtime = ToolRuntime::new_for_tests()
        .with_model_surface(surface)
        .with_communication_database(db.clone());
    (temp, db, runtime)
}

async fn handle_with_server_apps_enabled(
    runtime: &ToolRuntime,
    request: JsonRpcRequest,
    auth: Option<&crate::auth::AuthContext>,
    enabled: bool,
) -> McpOutcome {
    let protocol_era = super::super::inferred_protocol_era(&request);
    super::super::handle_mcp_request_with_lifecycle(
        runtime,
        None,
        request,
        auth,
        protocol_era,
        super::super::HostFileImportTrust::Untrusted,
        None,
        None,
        None,
        crate::model_surface::effective_mcp_compact_schemas(
            runtime.runtime_exposure(),
            crate::config::mcp_compact_schemas_override(),
        ),
        enabled,
        None,
    )
    .await
}

fn create_agent(
    runtime: &ToolRuntime,
    auth: &crate::auth::AuthContext,
    handle: &str,
    display_name: &str,
    key: &str,
) -> String {
    let result = runtime.create_agent_identity(
        Some(auth),
        handle.to_string(),
        display_name.to_string(),
        Some("PRIVATE Agent description".to_string()),
        vec!["PRIVATE-specialty-label".to_string()],
        key.to_string(),
    );
    assert!(result.success, "{:?}", result.output);
    result.output["agent"]["agent_id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn attach(
    runtime: &ToolRuntime,
    auth: &crate::auth::AuthContext,
    agent_id: &str,
    key: &str,
) -> (String, i64) {
    let result = runtime.attach_agent_endpoint(
        Some(auth),
        agent_id.to_string(),
        "ChatGPT".to_string(),
        Some(format!("attachment-{key}")),
        key.to_string(),
    );
    assert!(result.success, "{:?}", result.output);
    (
        result.output["endpoint"]["endpoint_id"]
            .as_str()
            .unwrap()
            .to_string(),
        result.output["endpoint"]["controller_generation"]
            .as_i64()
            .unwrap(),
    )
}

fn create_conversation(
    runtime: &ToolRuntime,
    auth: &crate::auth::AuthContext,
    a: &str,
    b: &str,
    key: &str,
) -> String {
    let result = runtime.create_conversation(
        Some(auth),
        Some("PRIVATE conversation title".to_string()),
        vec![a.to_string(), b.to_string()],
        key.to_string(),
    );
    assert!(result.success, "{:?}", result.output);
    result.output["conversation"]["conversation"]["conversation_id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[allow(clippy::too_many_arguments)]
fn post_message(
    runtime: &ToolRuntime,
    auth: &crate::auth::AuthContext,
    conversation_id: &str,
    sender: &str,
    sender_endpoint: &str,
    sender_generation: i64,
    receiver: &str,
    body: &str,
    key: &str,
) {
    let result = runtime.post_conversation_message(
        Some(auth),
        conversation_id.to_string(),
        body.to_string(),
        Some(sender.to_string()),
        Some(sender_endpoint.to_string()),
        Some(sender_generation),
        Some(vec![receiver.to_string()]),
        None,
        Some(key.to_string()),
        None,
        None,
    );
    assert!(result.success, "{:?}", result.output);
}

#[tokio::test]
async fn agent_continuation_app_surface_is_sparse_app_only_and_resource_backed() {
    let (_temp, _db, adaptive) = continuation_runtime(ModelSurface::AdaptiveRuntime);
    let auth = continuation_auth("continuation-surface");
    let ui = handle_with_server_apps_enabled(
        &adaptive,
        rpc(
            "tools/list",
            Some(json!(5101)),
            mcp_2026_ui_params(json!({})),
        ),
        Some(&auth),
        true,
    )
    .await;
    let McpOutcome::Ok(ui) = ui else {
        panic!("expected UI tools/list")
    };
    let present = tool(&ui["result"], "present_agent_continuation")
        .expect("present_agent_continuation must remain the sole card-creating entry");
    assert_eq!(
        present.pointer("/_meta/ui/resourceUri"),
        Some(&json!(MCP_AGENT_CONTINUATION_UI_RESOURCE_URI))
    );
    assert!(present.pointer("/_meta/ui/visibility").is_none());
    for name in APP_TOOLS {
        let descriptor = tool(&ui["result"], name).unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(
            descriptor.pointer("/_meta/ui/visibility"),
            Some(&json!(["app"]))
        );
        assert!(
            descriptor.pointer("/_meta/ui/resourceUri").is_none(),
            "{name} must coordinate the existing card rather than create another one"
        );
    }
    assert_eq!(
        tool(&ui["result"], "agent_continuation_bind").unwrap()["inputSchema"]["required"],
        json!(["agent_id", "endpoint_id", "expected_controller_generation"])
    );
    assert_eq!(
        tool(&ui["result"], "agent_continuation_wake_prepare").unwrap()["inputSchema"]["required"],
        json!([
            "agent_id",
            "endpoint_id",
            "expected_controller_generation",
            "binding_id",
            "wake_id",
            "attempt_id"
        ])
    );

    let plain = handle_with_server_apps_enabled(
        &adaptive,
        rpc("tools/list", Some(json!(5102)), mcp_2026_params(json!({}))),
        Some(&auth),
        true,
    )
    .await;
    let McpOutcome::Ok(plain) = plain else {
        panic!("expected plain tools/list")
    };
    assert!(tool(&plain["result"], "present_agent_continuation").is_some());
    assert!(tool(&plain["result"], "present_agent_continuation")
        .unwrap()
        .pointer("/_meta/ui/resourceUri")
        .is_none());
    for name in APP_TOOLS {
        assert!(tool(&plain["result"], name).is_none());
    }

    let disabled = handle_with_server_apps_enabled(
        &adaptive,
        rpc(
            "tools/list",
            Some(json!(5103)),
            mcp_2026_ui_params(json!({})),
        ),
        Some(&auth),
        false,
    )
    .await;
    let McpOutcome::Ok(disabled) = disabled else {
        panic!("expected disabled tools/list")
    };
    assert!(tool(&disabled["result"], "present_agent_continuation")
        .expect("presentation remains ordinary bounded read")
        .pointer("/_meta/ui/resourceUri")
        .is_none());
    for name in APP_TOOLS {
        assert!(tool(&disabled["result"], name).is_none());
    }

    let legacy = handle_with_server_apps_enabled(
        &adaptive,
        rpc("tools/list", Some(json!(5104)), json!({})),
        Some(&auth),
        true,
    )
    .await;
    let McpOutcome::Ok(legacy) = legacy else {
        panic!("expected legacy tools/list")
    };
    for name in APP_TOOLS {
        assert!(tool(&legacy["result"], name).is_none());
    }

    let (_local_temp, _local_db, local) = continuation_runtime(ModelSurface::LocalCoding);
    let local_ui = handle_with_server_apps_enabled(
        &local,
        rpc(
            "tools/list",
            Some(json!(5105)),
            mcp_2026_ui_params(json!({})),
        ),
        Some(&auth),
        true,
    )
    .await;
    let McpOutcome::Ok(local_ui) = local_ui else {
        panic!("expected Local Coding tools/list")
    };
    for name in APP_TOOLS {
        assert!(tool(&local_ui["result"], name).is_none());
    }

    for name in APP_TOOLS {
        assert!(
            !super::super::tools::adaptive_runtime_gateway_target_admitted_for_test(name, true),
            "{name} must not be an Adaptive generic gateway target"
        );
    }

    let resources = handle_with_server_apps_enabled(
        &adaptive,
        rpc(
            "resources/list",
            Some(json!(5106)),
            mcp_2026_ui_params(json!({})),
        ),
        Some(&auth),
        true,
    )
    .await;
    let McpOutcome::Ok(resources) = resources else {
        panic!("expected resources/list")
    };
    let resource = resources["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|resource| resource["uri"] == MCP_AGENT_CONTINUATION_UI_RESOURCE_URI)
        .expect("Agent Continuation resource");
    assert_eq!(resource["mimeType"], MCP_UI_RESOURCE_MIME_TYPE);
    let read = handle_with_server_apps_enabled(
        &adaptive,
        rpc(
            "resources/read",
            Some(json!(5107)),
            mcp_2026_ui_params(json!({"uri": MCP_AGENT_CONTINUATION_UI_RESOURCE_URI})),
        ),
        Some(&auth),
        true,
    )
    .await;
    let McpOutcome::Ok(read) = read else {
        panic!("expected resource read")
    };
    assert_eq!(
        read["result"]["contents"][0]["text"],
        MCP_AGENT_CONTINUATION_APP_HTML
    );
    assert!(
        MCP_AGENT_CONTINUATION_APP_HTML.contains("^wc_dagent_[0-9a-f]{32}$"),
        "App must validate the canonical durable Agent id prefix"
    );
    assert!(
        !MCP_AGENT_CONTINUATION_APP_HTML.contains("^wc_agent_[0-9a-f]{32}$"),
        "App must not accept the obsolete/nonexistent wc_agent_ prefix"
    );
    for required in [
        "ui/initialize",
        "agent_continuation_bind",
        "agent_continuation_state",
        "agent_continuation_wake_acquire",
        "agent_continuation_wake_prepare",
        "ui/message",
        "agent_continuation_wake_finish",
        "visibilitychange",
        "pagehide",
        "beforeunload",
        "ui/resource-teardown",
        "delivery_unknown",
    ] {
        assert!(
            MCP_AGENT_CONTINUATION_APP_HTML.contains(required),
            "missing {required}"
        );
    }
    for forbidden in [
        "localStorage",
        "console.log",
        "claim_fence",
        "Message body:",
    ] {
        assert!(
            !MCP_AGENT_CONTINUATION_APP_HTML.contains(forbidden),
            "App source contains forbidden marker {forbidden}"
        );
    }
    assert!(
        MCP_AGENT_CONTINUATION_APP_HTML.contains("function toolCallSucceeded(result)"),
        "App must distinguish a successful JSON-RPC exchange from a failed ToolResult"
    );
    assert!(
        MCP_AGENT_CONTINUATION_APP_HTML.contains(
            "if (!toolCallSucceeded(response)) throw new Error(\"Continuation finish was not accepted\")"
        ),
        "post-fence finish must remain pending when the tool returns business failure"
    );
    assert!(
        MCP_AGENT_CONTINUATION_APP_HTML.contains(
            "if (!toolCallSucceeded(response)) throw new Error(\"Continuation acquire was not accepted\")"
        ),
        "acquire must not reinterpret a business failure as no pending Wake"
    );
}

#[tokio::test]
async fn app_private_binding_and_consume_envelope_never_enter_structured_content() {
    let (_temp, _db, runtime) = continuation_runtime(ModelSurface::AdaptiveRuntime);
    let owner = continuation_auth("continuation-owner");
    let foreign = continuation_auth("continuation-foreign");
    let sender = create_agent(
        &runtime,
        &owner,
        "continuation-sender",
        "Sender",
        "continuation-sender-create",
    );
    let receiver = create_agent(
        &runtime,
        &owner,
        "continuation-receiver",
        "Receiver",
        "continuation-receiver-create",
    );
    let (sender_endpoint, sender_generation) =
        attach(&runtime, &owner, &sender, "continuation-sender-endpoint");
    let (receiver_endpoint, receiver_generation) = attach(
        &runtime,
        &owner,
        &receiver,
        "continuation-receiver-endpoint",
    );
    let conversation = create_conversation(
        &runtime,
        &owner,
        &sender,
        &receiver,
        "continuation-conversation",
    );

    let present = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(5201)),
            mcp_2026_ui_params(json!({
                "name": "present_agent_continuation",
                "arguments": {
                    "agent_id": receiver,
                    "endpoint_id": receiver_endpoint,
                    "expected_controller_generation": receiver_generation
                }
            })),
        ),
        Some(&owner),
        true,
    )
    .await;
    let McpOutcome::Ok(present) = present else {
        panic!("presentation failed")
    };
    assert_eq!(present["result"]["structuredContent"]["success"], true);
    assert_eq!(
        present["result"]["structuredContent"]["output"]["agent_continuation"]["display_name"],
        "Receiver"
    );
    let present_text = present["result"]["structuredContent"].to_string();
    assert!(!present_text.contains("PRIVATE Agent description"));
    assert!(!present_text.contains("PRIVATE-specialty-label"));

    let foreign_bind = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(5202)),
            mcp_2026_params(json!({
                "name": "agent_continuation_bind",
                "arguments": {
                    "agent_id": receiver,
                    "endpoint_id": receiver_endpoint,
                    "expected_controller_generation": receiver_generation
                }
            })),
        ),
        Some(&foreign),
        true,
    )
    .await;
    let McpOutcome::Ok(foreign_bind) = foreign_bind else {
        panic!("foreign call should be a tool failure")
    };
    assert_eq!(
        foreign_bind["result"]["structuredContent"]["success"],
        false
    );
    assert!(foreign_bind["result"]["_meta"]
        .get("webcodex/agentContinuation")
        .is_none());

    let bind = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(5203)),
            mcp_2026_params(json!({
                "name": "agent_continuation_bind",
                "arguments": {
                    "agent_id": receiver,
                    "endpoint_id": receiver_endpoint,
                    "expected_controller_generation": receiver_generation
                }
            })),
        ),
        Some(&owner),
        true,
    )
    .await;
    let McpOutcome::Ok(bind) = bind else {
        panic!("bind failed")
    };
    assert_eq!(bind["result"]["structuredContent"]["success"], true);
    let binding_id = bind["result"]["_meta"]["webcodex/agentContinuation"]["binding_id"]
        .as_str()
        .expect("private binding id")
        .to_string();
    assert!(binding_id.starts_with("wc_host_binding_"));
    let bind_structured = bind["result"]["structuredContent"].to_string();
    assert!(!bind_structured.contains("wc_host_binding_"));
    assert!(!bind_structured.contains("_app_private"));

    let private_body = "PRIVATE durable business message for MCP carrier";
    post_message(
        &runtime,
        &owner,
        &conversation,
        &sender,
        &sender_endpoint,
        sender_generation,
        &receiver,
        private_body,
        "continuation-private-message",
    );
    let acquire = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(5204)),
            mcp_2026_params(json!({
                "name": "agent_continuation_wake_acquire",
                "arguments": {
                    "agent_id": receiver,
                    "endpoint_id": receiver_endpoint,
                    "expected_controller_generation": receiver_generation,
                    "binding_id": binding_id
                }
            })),
        ),
        Some(&owner),
        true,
    )
    .await;
    let McpOutcome::Ok(acquire) = acquire else {
        panic!("acquire failed")
    };
    let wake_id = acquire["result"]["structuredContent"]["output"]["wake"]["wake_id"]
        .as_str()
        .unwrap()
        .to_string();
    let attempt_id = acquire["result"]["structuredContent"]["output"]["wake"]["attempt_id"]
        .as_str()
        .unwrap()
        .to_string();
    let acquire_text = acquire["result"]["structuredContent"].to_string();
    assert!(!acquire_text.contains("claim_fence"));
    assert!(!acquire_text.contains("consume_token"));
    assert!(!acquire_text.contains(private_body));

    let prepare = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(5205)),
            mcp_2026_params(json!({
                "name": "agent_continuation_wake_prepare",
                "arguments": {
                    "agent_id": receiver,
                    "endpoint_id": receiver_endpoint,
                    "expected_controller_generation": receiver_generation,
                    "binding_id": binding_id,
                    "wake_id": wake_id,
                    "attempt_id": attempt_id
                }
            })),
        ),
        Some(&owner),
        true,
    )
    .await;
    let McpOutcome::Ok(prepare) = prepare else {
        panic!("prepare failed")
    };
    assert_eq!(prepare["result"]["structuredContent"]["success"], true);
    let structured = prepare["result"]["structuredContent"].to_string();
    for forbidden in [
        "wc_host_binding_",
        "wc_wake_consume_",
        "claim_fence",
        private_body,
        "_app_private",
        "automatic_message",
    ] {
        assert!(
            !structured.contains(forbidden),
            "structuredContent leaked {forbidden}"
        );
    }
    let automatic_message = prepare["result"]["_meta"]["webcodex/agentContinuation"]
        ["automatic_message"]
        .as_str()
        .expect("App-private exact continuation envelope");
    assert!(automatic_message.contains("consume_token=wc_wake_consume_"));
    assert!(automatic_message.contains(&format!("agent_id={receiver}")));
    assert!(automatic_message.contains(&format!("endpoint_id={receiver_endpoint}")));
    assert!(automatic_message.contains(&format!("wake_id={wake_id}")));
    assert!(!automatic_message.contains(private_body));
    assert!(!automatic_message.contains("PRIVATE Agent description"));
    assert!(!automatic_message.contains("PRIVATE-specialty-label"));
}

#[tokio::test]
async fn agent_continuation_hidden_kernel_entry_is_fail_closed_without_protocol_capability() {
    use crate::tool_runtime::kernel::{
        HostFileImportTrust, ToolCallContext, ToolCallErrorStatus, ToolCallRequest,
        ToolProtocolCapabilities, ToolTransport,
    };

    let runtime =
        ToolRuntime::new_for_tests().with_model_surface(ModelSurface::FullOperatorRuntime);
    let auth = continuation_auth("continuation-kernel-gate");
    let outcome = runtime
        .call_tool_with_protocol_capabilities(
            ToolCallRequest {
                tool_name: "agent_continuation_bind".to_string(),
                arguments: json!({
                    "agent_id": format!("wc_dagent_{}", "a".repeat(32)),
                    "endpoint_id": format!("wc_endpoint_{}", "b".repeat(32)),
                    "expected_controller_generation": 1
                }),
            },
            ToolCallContext {
                transport: ToolTransport::Mcp,
                session_id: None,
                auth: Some(&auth),
                window: None,
                record_oauth_scope_denials: false,
                host_file_import_trust: HostFileImportTrust::Untrusted,
            },
            ToolProtocolCapabilities::default(),
        )
        .await;
    assert!(matches!(
        outcome.error_status,
        Some(ToolCallErrorStatus::InvalidArguments { ref message })
            if message.contains("Agent continuation App coordination")
    ));
    assert!(outcome.result.is_none());
}
