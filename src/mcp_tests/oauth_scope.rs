use super::*;

fn oauth_mcp_service_with_surface(
    scopes: &str,
    model_surface: ModelSurface,
) -> (tempfile::TempDir, Service, String) {
    let config = test_config_oauth2(Some("secret"));
    let (tmp, db) = test_db();
    let user = seed_user(&db, "alice");
    let client = seed_oauth_client(&db, &user);
    let token = seed_oauth_access_token(&db, &client, &user, scopes);
    let runtime = Arc::new(test_runtime_with_surface(model_surface));
    let service = Service::new(build_test_router(config, db, runtime));
    (tmp, service, token)
}

fn oauth_mcp_service(scopes: &str) -> (tempfile::TempDir, Service, String) {
    oauth_mcp_service_with_surface(scopes, ModelSurface::LocalCoding)
}

async fn oauth_mcp_service_with_plugin_runner(
    scopes: &str,
) -> (tempfile::TempDir, Service, String) {
    let config = test_config_oauth2(Some("secret"));
    let (tmp, db) = test_db();
    let user = seed_user(&db, "alice");
    let client = seed_oauth_client(&db, &user);
    let token = seed_oauth_access_token(&db, &client, &user, scopes);
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::LocalCoding));
    let mut capabilities = RunnerCapabilities::default();
    capabilities.native_tool_plugins = true;
    runtime
        .runner_registry
        .register(crate::test_support::current_runner_registration(
            RunnerRegisterRequest {
                client_id: "oauth-plugin-runner".to_string(),
                runner_instance_id: "oauth-plugin-runner-instance".to_string(),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: Some("OAuth Plugin Runner".to_string()),
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
    let service = Service::new(build_test_router(config, db, runtime));
    (tmp, service, token)
}

fn assert_mcp_oauth_scope_rejected(
    status: StatusCode,
    body: &Value,
    challenge: Option<&str>,
    scope: Option<&str>,
) {
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {:?}", body);
    assert_eq!(body["error"], "insufficient_scope");
    let challenge = challenge.unwrap_or("");
    assert!(
        challenge.contains("error=\"insufficient_scope\""),
        "challenge: {}",
        challenge
    );
    if let Some(scope) = scope {
        assert!(
            body["error_description"]
                .as_str()
                .unwrap_or("")
                .contains(scope),
            "body: {:?}",
            body
        );
        assert!(challenge.contains(scope), "challenge: {}", challenge);
    }
}

#[tokio::test]
async fn pat_mcp_tools_list_requires_runtime_read_without_oauth_framing() {
    let runtime = test_runtime();
    let mut auth = mcp_export_api_auth("pat-project-read-only", "alice");
    auth.scopes = vec![crate::auth::SCOPE_PROJECT_READ.to_string()];
    let outcome = handle_mcp_request(
        &runtime,
        rpc("tools/list", Some(json!(41)), json!({})),
        Some(&auth),
    )
    .await;

    match outcome {
        McpOutcome::Forbidden {
            body,
            required_scope,
        } => {
            assert_eq!(required_scope, Some(crate::auth::SCOPE_RUNTIME_READ));
            assert_eq!(body["status"], StatusCode::FORBIDDEN.as_u16());
            assert_ne!(body["error"], "insufficient_scope");
            assert!(body["error"]
                .as_str()
                .unwrap_or("")
                .contains(crate::auth::SCOPE_RUNTIME_READ));
        }
        other => panic!("PAT without runtime:read must fail closed, got {other:?}"),
    }
}

#[tokio::test]
async fn oauth2_mcp_tools_list_requires_runtime_read() {
    let (_tmp, service, token) = oauth_mcp_service("runtime:read");
    let (status, body, _) = oauth_mcp_request(&service, &token, "tools/list", json!({})).await;
    assert_eq!(status, StatusCode::OK, "body: {:?}", body);

    let (_tmp, service, token) = oauth_mcp_service("project:read");
    let (status, body, challenge) =
        oauth_mcp_request(&service, &token, "tools/list", json!({})).await;
    assert_mcp_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_RUNTIME_READ),
    );
}

#[tokio::test]
async fn oauth2_mcp_local_gateway_catalog_and_call_require_explicit_scope() {
    let (_tmp, service, token) = oauth_mcp_service("runtime:read");
    let (status, body, _) = oauth_mcp_request(&service, &token, "tools/list", json!({})).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert!(!body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == crate::mcp_gateway::MCP_TOOL_NAME));

    let (status, body, challenge) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": crate::mcp_gateway::MCP_TOOL_NAME,
            "arguments": {"action": "list"}
        }),
    )
    .await;
    assert_mcp_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_MCP_LOCAL),
    );

    let (_tmp, service, token) = oauth_mcp_service("runtime:read mcp:local");
    let (status, body, _) = oauth_mcp_request(&service, &token, "tools/list", json!({})).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert!(body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == crate::mcp_gateway::MCP_TOOL_NAME));

    let (status, body, _) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": crate::mcp_gateway::MCP_TOOL_NAME,
            "arguments": {"action": "list"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["result"]["isError"], false);
    assert_eq!(body["result"]["structuredContent"]["servers"], json!([]));
}

#[tokio::test]
async fn oauth2_native_plugin_catalog_and_call_require_explicit_plugin_scope() {
    let (_tmp, service, token) = oauth_mcp_service("runtime:read");
    let (status, body, _) = oauth_mcp_request(&service, &token, "tools/list", json!({})).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert!(!body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == crate::plugin_gateway::PLUGIN_TOOL_NAME));

    let (status, body, challenge) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": crate::plugin_gateway::PLUGIN_TOOL_NAME,
            "arguments": {"action": "list"}
        }),
    )
    .await;
    assert_mcp_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_PLUGIN_INSPECT),
    );

    let (_tmp, service, token) = oauth_mcp_service("runtime:read plugin:inspect plugin:invoke");
    let (status, body, _) = oauth_mcp_request(&service, &token, "tools/list", json!({})).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert!(body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == crate::plugin_gateway::PLUGIN_TOOL_NAME));

    let (status, body, _) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": crate::plugin_gateway::PLUGIN_TOOL_NAME,
            "arguments": {"action": "list"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["result"]["isError"], false);
    assert_eq!(body["result"]["structuredContent"]["runners"], json!([]));

    let (status, body, _) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": crate::plugin_gateway::PLUGIN_TOOL_NAME,
            "arguments": {
                "action": "call",
                "binding": "wc_pbind_00000000000000000000000000000000",
                "arguments": {"value": "hello"}
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["result"]["isError"], true);
    assert_eq!(
        body["result"]["structuredContent"]["error"]["code"], "describe_required",
        "a syntactically valid call must reach binding resolution after plugin:invoke scope"
    );

    let (status, body, challenge) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": crate::plugin_gateway::PLUGIN_TOOL_NAME,
            "arguments": {"action": "check", "runner": "runner-a", "plugin": "repo-tools"}
        }),
    )
    .await;
    assert_mcp_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_PLUGIN_MANAGE),
    );
}

#[tokio::test]
async fn oauth2_managed_ssh_resource_surface_requires_explicit_ssh_local_scope() {
    let (_tmp, service, token) = oauth_mcp_service("runtime:read project:write job:run");
    let (status, body, _) = oauth_mcp_request(&service, &token, "tools/list", json!({})).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert!(!body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == crate::ssh_resource_gateway::SSH_RESOURCE_TOOL_NAME));

    let (status, body, challenge) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": crate::ssh_resource_gateway::SSH_RESOURCE_TOOL_NAME,
            "arguments": {"action": "list", "runner": "runner-a"}
        }),
    )
    .await;
    assert_mcp_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_SSH_LOCAL),
    );

    let (_tmp, service, token) = oauth_mcp_service("runtime:read ssh:local");
    let (status, body, _) = oauth_mcp_request(&service, &token, "tools/list", json!({})).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert!(body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == crate::ssh_resource_gateway::SSH_RESOURCE_TOOL_NAME));

    let (status, body, _) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": crate::ssh_resource_gateway::SSH_RESOURCE_TOOL_NAME,
            "arguments": {"action": "list", "runner": "runner-a"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["result"]["isError"], true);
    assert_eq!(
        body["result"]["structuredContent"]["error"]["code"],
        "ssh_resource_registry_unavailable"
    );
}

#[tokio::test]
async fn oauth2_plugin_gateway_visibility_uses_any_plugin_scope_and_provider_names_stay_hidden() {
    let tool_name = "oauth_plugin_echo";
    let (_tmp, service, token) = oauth_mcp_service_with_plugin_runner("runtime:read").await;
    let (status, body, _) = oauth_mcp_request(&service, &token, "tools/list", json!({})).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    let names = listed_tool_names(&body);
    assert!(!names.contains(crate::plugin_gateway::PLUGIN_TOOL_NAME));
    assert!(!names.contains(tool_name));

    for scope in [
        crate::auth::SCOPE_PLUGIN_INSPECT,
        crate::auth::SCOPE_PLUGIN_INVOKE,
        crate::auth::SCOPE_PLUGIN_MANAGE,
    ] {
        let scopes = format!("runtime:read {scope}");
        let (_tmp, service, token) = oauth_mcp_service_with_plugin_runner(&scopes).await;
        let (status, body, _) = oauth_mcp_request(&service, &token, "tools/list", json!({})).await;
        assert_eq!(status, StatusCode::OK, "scope={scope}, body={body:?}");
        let names = listed_tool_names(&body);
        assert!(
            names.contains(crate::plugin_gateway::PLUGIN_TOOL_NAME),
            "scope={scope} must make the stable Plugin gateway visible"
        );
        assert!(
            !names.contains(tool_name),
            "provider-local names must never become outer MCP tools"
        );
    }
}

#[tokio::test]
async fn oauth2_adaptive_gateway_preserves_canonical_target_scope_errors() {
    let (_tmp, service, token) =
        oauth_mcp_service_with_surface("runtime:read", ModelSurface::AdaptiveRuntime);

    let (status, body, _) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
            "arguments": {
                "tool": "list_tools",
                "arguments": {"summary_only": true, "limit": 1}
            }
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "authorized gateway dispatch: {body:?}"
    );
    assert_eq!(body["result"]["isError"], false);
    assert_eq!(
        body["result"]["structuredContent"]["output"]["returned_count"],
        1
    );

    let (status, body, challenge) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
            "arguments": {
                "tool": "read_files",
                "arguments": {"project": "demo", "items": [{"path": "README.md"}]}
            }
        }),
    )
    .await;
    assert_mcp_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_PROJECT_READ),
    );

    let (status, body, challenge) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
            "arguments": {
                "tool": crate::mcp_gateway::MCP_TOOL_NAME,
                "arguments": {"action": "list"}
            }
        }),
    )
    .await;
    assert_mcp_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_MCP_LOCAL),
    );

    let (status, body, challenge) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": crate::plugin_gateway::PLUGIN_TOOL_NAME,
            "arguments": {"action": "list"}
        }),
    )
    .await;
    assert_mcp_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_PLUGIN_INSPECT),
    );

    let (status, body, _) = oauth_mcp_request(&service, &token, "tools/list", json!({})).await;
    assert_eq!(status, StatusCode::OK, "adaptive tools/list: {body:?}");
    let names = listed_tool_names(&body);
    assert!(names.contains(crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME));
    assert!(!names.contains(crate::mcp_gateway::MCP_TOOL_NAME));

    let (_tmp, service, token) =
        oauth_mcp_service_with_surface("runtime:read mcp:local", ModelSurface::AdaptiveRuntime);
    let (status, body, _) = oauth_mcp_request(&service, &token, "tools/list", json!({})).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "authorized adaptive tools/list: {body:?}"
    );
    let names = listed_tool_names(&body);
    assert!(names.contains(crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME));
    assert!(names.contains(crate::mcp_gateway::MCP_TOOL_NAME));

    let (status, body, _) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
            "arguments": {
                "tool": crate::mcp_gateway::MCP_TOOL_NAME,
                "arguments": {"action": "list"}
            }
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "authorized adaptive mcp_tool: {body:?}"
    );
    assert_eq!(body["result"]["isError"], false);
    assert_eq!(body["result"]["structuredContent"]["servers"], json!([]));
}

#[tokio::test]
async fn oauth2_mcp_computer_app_resources_require_runtime_read() {
    let (_tmp, service, token) =
        oauth_mcp_service_with_surface("runtime:read", ModelSurface::FullOperatorRuntime);
    let (status, body, _) = oauth_mcp_request(
        &service,
        &token,
        "resources/list",
        mcp_2026_ui_params(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(
        body["result"]["resources"][0]["uri"],
        MCP_COMPUTER_UI_RESOURCE_URI
    );

    let (_tmp, service, token) =
        oauth_mcp_service_with_surface("project:read", ModelSurface::FullOperatorRuntime);
    let (status, body, challenge) = oauth_mcp_request(
        &service,
        &token,
        "resources/list",
        mcp_2026_ui_params(json!({})),
    )
    .await;
    assert_mcp_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_RUNTIME_READ),
    );
}

#[tokio::test]
async fn oauth2_mcp_computer_snapshot_keeps_computer_read_scope() {
    let arguments = json!({
        "client_id": "missing-runner",
        "surface_id": "surface_test"
    });
    let (_tmp, service, token) =
        oauth_mcp_service_with_surface("runtime:read", ModelSurface::FullOperatorRuntime);
    let (status, body, challenge) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({ "name": "computer_snapshot", "arguments": arguments.clone() }),
    )
    .await;
    assert_mcp_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_COMPUTER_READ),
    );

    let (_tmp, service, token) =
        oauth_mcp_service_with_surface("computer:read", ModelSurface::FullOperatorRuntime);
    let (status, body, _) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({ "name": "computer_snapshot", "arguments": arguments }),
    )
    .await;
    assert_ne!(status, StatusCode::FORBIDDEN, "body: {body:?}");
}

#[tokio::test]
async fn oauth2_mcp_server_discover_requires_runtime_read_and_advertises_both_versions() {
    let (_tmp, service, token) = oauth_mcp_service("runtime:read");
    let (status, body, _) = oauth_mcp_request(
        &service,
        &token,
        "server/discover",
        json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": MCP_STATELESS_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(
        body["result"]["supportedVersions"],
        json!([
            MCP_STATELESS_PROTOCOL_VERSION,
            MCP_CHATGPT_PROTOCOL_VERSION,
            MCP_PROTOCOL_VERSION
        ])
    );
    assert_eq!(body["result"]["resultType"], "complete");

    let (_tmp, service, token) = oauth_mcp_service("project:read");
    let (status, body, challenge) = oauth_mcp_request(
        &service,
        &token,
        "server/discover",
        mcp_2026_params(json!({})),
    )
    .await;
    assert_mcp_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_RUNTIME_READ),
    );
}

#[tokio::test]
async fn oauth2_mcp_unknown_method_keeps_legacy_fail_closed_but_modern_returns_404() {
    let (_tmp, service, token) = oauth_mcp_service("runtime:read");

    let (legacy_status, legacy_body, legacy_challenge) =
        oauth_mcp_request(&service, &token, "prompts/list", json!({})).await;
    assert_mcp_oauth_scope_rejected(
        legacy_status,
        &legacy_body,
        legacy_challenge.as_deref(),
        None,
    );

    let (modern_status, modern_body, _) =
        oauth_mcp_request(&service, &token, "prompts/list", mcp_2026_params(json!({}))).await;
    assert_eq!(
        modern_status,
        StatusCode::NOT_FOUND,
        "body: {modern_body:?}"
    );
    assert_eq!(modern_body["error"]["code"], -32601);
}

#[tokio::test]
async fn oauth2_mcp_tool_call_requires_project_read_for_read_files() {
    let (_tmp, service, token) = oauth_mcp_service("project:read");
    let (status, body, _) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({"name": "read_files", "arguments": {"project": "demo", "items": [{"path": "README.md"}]}}),
    )
    .await;
    assert_ne!(status, StatusCode::FORBIDDEN, "body: {:?}", body);

    let (_tmp, service, token) = oauth_mcp_service("runtime:read");
    let (status, body, challenge) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({"name": "read_files", "arguments": {"project": "demo", "items": [{"path": "README.md"}]}}),
    )
    .await;
    assert_mcp_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_PROJECT_READ),
    );
}

#[tokio::test]
async fn oauth2_mcp_tool_call_requires_project_write_for_edit_tools() {
    // Edit tools require the project:write scope. Select the explicit full
    // operator surface so the scope gate (not the local_coding boundary)
    // decides this call.
    let (_tmp, service, token) =
        oauth_mcp_service_with_surface("project:write", ModelSurface::FullOperatorRuntime);
    let (status, body, _) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": "write_project_file",
            "arguments": {
                "project": "demo",
                "path": "README.md",
                "content": "new"
            }
        }),
    )
    .await;
    assert_ne!(status, StatusCode::FORBIDDEN, "body: {:?}", body);

    let (_tmp, service, token) =
        oauth_mcp_service_with_surface("project:read", ModelSurface::FullOperatorRuntime);
    let (status, body, challenge) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": "write_project_file",
            "arguments": {
                "project": "demo",
                "path": "README.md",
                "content": "new"
            }
        }),
    )
    .await;
    assert_mcp_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_PROJECT_WRITE),
    );
}

#[tokio::test]
async fn oauth2_mcp_tool_call_requires_job_run_for_run_shell() {
    let (_tmp, service, token) = oauth_mcp_service("job:run");
    let (status, body, _) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({"name": "run_shell", "arguments": {"project": "demo", "command": "echo hi"}}),
    )
    .await;
    assert_ne!(status, StatusCode::FORBIDDEN, "body: {:?}", body);

    let (_tmp, service, token) = oauth_mcp_service("project:read");
    let (status, body, challenge) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({"name": "run_shell", "arguments": {"project": "demo", "command": "echo hi"}}),
    )
    .await;
    assert_mcp_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_JOB_RUN),
    );
}

#[tokio::test]
async fn oauth2_mcp_detached_process_requires_job_run_and_job_detach() {
    for (scopes, missing) in [
        ("job:run", crate::auth::SCOPE_JOB_DETACH),
        ("job:detach", crate::auth::SCOPE_JOB_RUN),
    ] {
        let (_tmp, service, token) =
            oauth_mcp_service_with_surface(scopes, ModelSurface::FullOperatorRuntime);
        let (status, body, challenge) = oauth_mcp_request(
            &service,
            &token,
            "tools/call",
            json!({
                "name": "run_detached_process",
                "arguments": {
                    "project": "demo",
                    "idempotency_key": "oauth-detached-scope",
                    "executable": "argv-helper",
                    "args": []
                }
            }),
        )
        .await;
        assert_mcp_oauth_scope_rejected(status, &body, challenge.as_deref(), Some(missing));
    }

    let (_tmp, service, token) =
        oauth_mcp_service_with_surface("job:run job:detach", ModelSurface::FullOperatorRuntime);
    let (status, body, _) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": "run_detached_process",
            "arguments": {
                "project": "demo",
                "idempotency_key": "oauth-detached-both",
                "executable": "argv-helper",
                "args": []
            }
        }),
    )
    .await;
    assert_ne!(status, StatusCode::FORBIDDEN, "body: {:?}", body);
}

#[tokio::test]
async fn oauth2_mcp_unknown_tool_fails_closed() {
    let (_tmp, service, token) = oauth_mcp_service_with_surface(
        "runtime:read project:read",
        ModelSurface::FullOperatorRuntime,
    );
    let (status, body, challenge) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({"name": "no_such_tool", "arguments": {}}),
    )
    .await;
    assert_mcp_oauth_scope_rejected(status, &body, challenge.as_deref(), None);
}

fn listed_tool_names(body: &Value) -> std::collections::HashSet<String> {
    body["result"]["tools"]
        .as_array()
        .expect("tools/list result")
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn oauth2_memory_tools_require_canonical_project_and_memory_scopes() {
    for (extra_scopes, expected) in [
        ("project:read", vec![]),
        ("memory:read", vec![]),
        (
            "project:read memory:read",
            vec!["memory_search", "memory_read"],
        ),
        (
            "project:write memory:manage",
            vec!["memory_set", "memory_delete"],
        ),
        ("project:write", vec![]),
        ("memory:manage", vec![]),
        (
            "project:read memory:read project:write memory:manage",
            vec![
                "memory_search",
                "memory_read",
                "memory_set",
                "memory_delete",
            ],
        ),
    ] {
        let scopes = format!("runtime:read {extra_scopes}");
        let (_tmp, service, token) =
            oauth_mcp_service_with_surface(&scopes, ModelSurface::FullOperatorRuntime);
        let (status, body, _) =
            oauth_mcp_request(&service, &token, "tools/list", mcp_2026_params(json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{scopes}: {body:?}");
        let names = listed_tool_names(&body);
        let actual = [
            "memory_search",
            "memory_read",
            "memory_set",
            "memory_delete",
        ]
        .into_iter()
        .filter(|name| names.contains(*name))
        .collect::<Vec<_>>();
        assert_eq!(actual, expected, "{scopes}");
    }

    for (scopes, tool, arguments, missing_scope) in [
        (
            "runtime:read project:read",
            "memory_search",
            json!({"project": "demo"}),
            crate::auth::SCOPE_MEMORY_READ,
        ),
        (
            "runtime:read memory:read",
            "memory_search",
            json!({"project": "demo"}),
            crate::auth::SCOPE_PROJECT_READ,
        ),
        (
            "runtime:read project:write",
            "memory_set",
            json!({"project":"demo","memory_key":"policy","summary":"summary"}),
            crate::auth::SCOPE_MEMORY_MANAGE,
        ),
        (
            "runtime:read memory:manage",
            "memory_set",
            json!({"project":"demo","memory_key":"policy","summary":"summary"}),
            crate::auth::SCOPE_PROJECT_WRITE,
        ),
    ] {
        let (_tmp, service, token) =
            oauth_mcp_service_with_surface(scopes, ModelSurface::FullOperatorRuntime);
        let (status, body, challenge) = oauth_mcp_request(
            &service,
            &token,
            "tools/call",
            mcp_2026_params(json!({"name":tool,"arguments":arguments})),
        )
        .await;
        assert_mcp_oauth_scope_rejected(status, &body, challenge.as_deref(), Some(missing_scope));
    }
}

#[tokio::test]
async fn oauth2_tools_list_projects_optional_computer_tools_from_actual_token_scopes() {
    let baseline = "runtime:read project:read project:write job:run computer:read computer:control";
    let (_tmp, service, token) =
        oauth_mcp_service_with_surface(baseline, ModelSurface::FullOperatorRuntime);
    let (status, body, _) = oauth_mcp_request(&service, &token, "tools/list", json!({})).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    let baseline_tools = listed_tool_names(&body);
    for hidden in [
        "computer_launch_application",
        "computer_list_displays",
        "computer_snapshot_display",
        "computer_pointer_move",
        "computer_pointer_click",
        "computer_read_clipboard",
        "computer_write_clipboard",
    ] {
        assert!(!baseline_tools.contains(hidden), "baseline leaked {hidden}");
    }

    for (extra_scopes, present, absent) in [
        (
            "computer:launch",
            vec!["computer_launch_application"],
            vec![
                "computer_list_displays",
                "computer_pointer_move",
                "computer_read_clipboard",
            ],
        ),
        (
            "computer:display_read",
            vec!["computer_list_displays", "computer_snapshot_display"],
            vec!["computer_pointer_move", "computer_read_clipboard"],
        ),
        (
            "computer:display_read computer:pointer_control",
            vec![
                "computer_list_displays",
                "computer_pointer_move",
                "computer_pointer_click",
            ],
            vec!["computer_read_clipboard", "computer_write_clipboard"],
        ),
        (
            "computer:clipboard_read",
            vec!["computer_read_clipboard"],
            vec!["computer_write_clipboard", "computer_pointer_move"],
        ),
        (
            "computer:clipboard_write",
            vec!["computer_write_clipboard"],
            vec!["computer_read_clipboard", "computer_pointer_move"],
        ),
    ] {
        let scopes = format!("{baseline} {extra_scopes}");
        let (_tmp, service, token) =
            oauth_mcp_service_with_surface(&scopes, ModelSurface::FullOperatorRuntime);
        let (status, body, _) = oauth_mcp_request(&service, &token, "tools/list", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{scopes}: {body:?}");
        let names = listed_tool_names(&body);
        for name in present {
            assert!(names.contains(name), "{scopes} should list {name}");
        }
        for name in absent {
            assert!(!names.contains(name), "{scopes} should hide {name}");
        }
    }
}

#[tokio::test]
async fn oauth2_coding_agent_tools_require_independent_scope_in_catalog_and_direct_call() {
    let insufficient = "runtime:read project:read project:write job:run mcp:local";
    let (_tmp, service, token) =
        oauth_mcp_service_with_surface(insufficient, ModelSurface::FullOperatorRuntime);
    let (status, body, _) = oauth_mcp_request(&service, &token, "tools/list", json!({})).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    let names = listed_tool_names(&body);
    for name in [
        "coding_agent_start",
        "coding_agent_observe",
        "coding_agent_cancel",
    ] {
        assert!(!names.contains(name), "insufficient scopes leaked {name}");
    }

    let (status, body, challenge) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": "coding_agent_cancel",
            "arguments": {"run_id": "wc_agent_run_scopeprobe0001"}
        }),
    )
    .await;
    assert_mcp_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_CODING_AGENT_RUN),
    );

    let coding_agent_without_write = "runtime:read project:read coding_agent:run mcp:local";
    let (_tmp, service, token) = oauth_mcp_service_with_surface(
        coding_agent_without_write,
        ModelSurface::FullOperatorRuntime,
    );
    let (status, body, _) = oauth_mcp_request(&service, &token, "tools/list", json!({})).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    let names = listed_tool_names(&body);
    assert!(names.contains("coding_agent_observe"));
    assert!(names.contains("coding_agent_cancel"));
    assert!(
        !names.contains("coding_agent_start"),
        "delegated start must also require project:write"
    );
    let (status, body, challenge) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": "coding_agent_start",
            "arguments": {
                "project": "agent:missing:demo",
                "provider_id": "codex",
                "idempotency_key": "scope-probe",
                "instruction": "inspect"
            }
        }),
    )
    .await;
    assert_mcp_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_PROJECT_WRITE),
    );

    let allowed = format!("{insufficient} coding_agent:run");
    let (_tmp, service, token) =
        oauth_mcp_service_with_surface(&allowed, ModelSurface::FullOperatorRuntime);
    let (status, body, _) = oauth_mcp_request(&service, &token, "tools/list", json!({})).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    let names = listed_tool_names(&body);
    for name in [
        "coding_agent_start",
        "coding_agent_observe",
        "coding_agent_cancel",
    ] {
        assert!(
            names.contains(name),
            "explicit coding_agent:run should list {name}"
        );
    }
}

#[tokio::test]
async fn oauth2_pointer_tool_call_still_requires_display_scope_even_if_invoked_directly() {
    let scopes = "runtime:read computer:read computer:control computer:pointer_control";
    let (_tmp, service, token) =
        oauth_mcp_service_with_surface(scopes, ModelSurface::FullOperatorRuntime);
    let (status, body, challenge) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": "computer_pointer_move",
            "arguments": {
                "client_id": "missing-runner",
                "display_id": "display_00000000000000000000000000000000",
                "snapshot_generation": 1,
                "x": 0,
                "y": 0
            }
        }),
    )
    .await;
    assert_mcp_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_COMPUTER_DISPLAY_READ),
    );
}

#[tokio::test]
async fn api_token_mcp_behavior_unchanged() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime());
    let service = Service::new(build_test_router(config, db, runtime));
    let mut resp = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 43,
            "method": "tools/call",
            "params": {"name": "no_such_tool", "arguments": {}}
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::BAD_REQUEST);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["error"]["code"], -32602);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("no_such_tool"));
}
