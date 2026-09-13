use super::*;

fn tool<'a>(payload: &'a Value, name: &str) -> Option<&'a Value> {
    payload["tools"]
        .as_array()?
        .iter()
        .find(|tool| tool["name"] == name)
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

#[tokio::test]
async fn work_result_descriptor_is_explicit_sparse_app_only_and_resource_backed() {
    assert_eq!(
        MCP_WORK_RESULT_UI_RESOURCE_URI,
        "ui://webcodex/work-result/v1"
    );
    let runtime = test_runtime_with_surface(ModelSurface::AdaptiveRuntime);

    let ui = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/list",
            Some(json!(5101)),
            mcp_2026_ui_params(json!({})),
        ),
        None,
        true,
    )
    .await;
    let McpOutcome::Ok(ui) = ui else {
        panic!("expected UI-capable adaptive tools/list");
    };
    let present = tool(&ui["result"], "present_work_result").expect("present_work_result");
    assert_eq!(
        present.pointer("/_meta/ui/resourceUri"),
        Some(&json!(MCP_WORK_RESULT_UI_RESOURCE_URI))
    );
    assert!(present.pointer("/_meta/ui/visibility").is_none());
    let state = tool(&ui["result"], "work_result_state").expect("app-only work_result_state");
    assert_eq!(state.pointer("/_meta/ui/visibility"), Some(&json!(["app"])));
    assert!(state.pointer("/_meta/ui/resourceUri").is_none());
    assert_eq!(
        state["inputSchema"]["required"],
        json!(["project", "session_id"])
    );

    let full = test_runtime_with_surface(ModelSurface::FullOperatorRuntime);
    let full_ui = handle_with_server_apps_enabled(
        &full,
        rpc(
            "tools/list",
            Some(json!(5104)),
            mcp_2026_ui_params(json!({})),
        ),
        None,
        true,
    )
    .await;
    let McpOutcome::Ok(full_ui) = full_ui else {
        panic!("expected UI-capable full tools/list");
    };
    for name in [
        "show_changes",
        "list_jobs",
        "observe_jobs",
        "cargo_check",
        "cargo_test",
        "validation_summary",
        "git_review_summary",
        "finish_coding_task",
    ] {
        let descriptor = tool(&full_ui["result"], name).unwrap_or_else(|| panic!("missing {name}"));
        assert_ne!(
            descriptor
                .pointer("/_meta/ui/resourceUri")
                .and_then(Value::as_str),
            Some(MCP_WORK_RESULT_UI_RESOURCE_URI),
            "{name} must not create a Work Result card"
        );
    }
    assert_eq!(
        tool(&full_ui["result"], "present_goal_plan")
            .unwrap()
            .pointer("/_meta/ui/resourceUri"),
        Some(&json!(MCP_GOAL_PLAN_UI_RESOURCE_URI))
    );
    assert_eq!(
        tool(&full_ui["result"], "present_agent_continuation")
            .unwrap()
            .pointer("/_meta/ui/resourceUri"),
        Some(&json!(MCP_AGENT_CONTINUATION_UI_RESOURCE_URI))
    );

    let plain = handle_with_server_apps_enabled(
        &runtime,
        rpc("tools/list", Some(json!(5102)), mcp_2026_params(json!({}))),
        None,
        true,
    )
    .await;
    let McpOutcome::Ok(plain) = plain else {
        panic!("ordinary tools/list failed");
    };
    assert!(tool(&plain["result"], "present_work_result").is_some());
    assert!(tool(&plain["result"], "present_work_result")
        .unwrap()
        .pointer("/_meta/ui/resourceUri")
        .is_none());
    assert!(tool(&plain["result"], "work_result_state").is_none());

    let disabled = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/list",
            Some(json!(5103)),
            mcp_2026_ui_params(json!({})),
        ),
        None,
        false,
    )
    .await;
    let McpOutcome::Ok(disabled) = disabled else {
        panic!("Apps-disabled tools/list failed");
    };
    assert!(tool(&disabled["result"], "work_result_state").is_none());
    assert!(tool(&disabled["result"], "present_work_result")
        .unwrap()
        .pointer("/_meta/ui/resourceUri")
        .is_none());

    assert!(!registered_tool_specs()
        .iter()
        .any(|spec| spec.name == "work_result_state"));
    assert!(
        !super::super::tools::adaptive_runtime_gateway_target_admitted_for_test(
            "work_result_state",
            true
        )
    );
}

#[tokio::test]
async fn work_result_resource_is_canonical_while_changes_resources_are_hidden_compatibility() {
    const PUBLIC_URL: &str = "https://self-host.example";
    let runtime =
        test_runtime_with_surface_and_public_url(ModelSurface::FullOperatorRuntime, PUBLIC_URL);
    let resources = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "resources/list",
            Some(json!(5110)),
            mcp_2026_ui_params(json!({})),
        ),
        None,
        true,
    )
    .await;
    let McpOutcome::Ok(resources) = resources else {
        panic!("resources/list failed");
    };
    let resources = resources["result"]["resources"].as_array().unwrap();
    assert!(resources
        .iter()
        .any(|resource| resource["uri"] == MCP_WORK_RESULT_UI_RESOURCE_URI));
    let work_resource = resources
        .iter()
        .find(|resource| resource["uri"] == MCP_WORK_RESULT_UI_RESOURCE_URI)
        .expect("canonical Work Result resource");
    let work_description = work_resource["description"].as_str().unwrap();
    assert!(work_description.contains("user explicitly refreshes"));
    assert!(!work_description.contains("poll"));
    assert!(!resources
        .iter()
        .any(|resource| resource["uri"] == MCP_RESULT_UI_RESOURCE_URI));
    for legacy in MCP_RESULT_UI_RESOURCE_LEGACY_URIS {
        assert!(!resources.iter().any(|resource| resource["uri"] == *legacy));
    }
    let read = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "resources/read",
            Some(json!(5111)),
            mcp_2026_ui_params(json!({"uri": MCP_WORK_RESULT_UI_RESOURCE_URI})),
        ),
        None,
        true,
    )
    .await;
    let McpOutcome::Ok(read) = read else {
        panic!("Work resource read failed");
    };
    assert_eq!(
        read["result"]["contents"][0]["text"],
        MCP_WORK_RESULT_APP_HTML
    );
    assert_eq!(
        read["result"]["contents"][0]["_meta"]["ui"]["domain"],
        PUBLIC_URL
    );
}

#[tokio::test]
async fn work_result_state_call_requires_app_protocol_capability() {
    let runtime = test_runtime_with_surface(ModelSurface::AdaptiveRuntime);
    let args = json!({
        "name": "work_result_state",
        "arguments": {
            "project": "agent:missing:project",
            "session_id": format!("wc_sess_{}", "1".repeat(32))
        }
    });
    let app = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(5120)),
            mcp_2026_ui_params(args.clone()),
        ),
        None,
        true,
    )
    .await;
    let McpOutcome::Ok(app) = app else {
        panic!("App-only state call should reach runtime under App capability");
    };
    assert_eq!(app["result"]["structuredContent"]["success"], false);

    for params in [mcp_2026_params(args.clone()), mcp_2026_ui_params(args)] {
        let outcome = handle_with_server_apps_enabled(
            &runtime,
            rpc("tools/call", Some(json!(5121)), params),
            None,
            false,
        )
        .await;
        assert!(matches!(outcome, McpOutcome::BadRequest(_)));
    }
}

#[tokio::test]
async fn work_result_state_discards_unadvertised_recording_session_wrapper() {
    let runtime = test_runtime_with_surface(ModelSurface::AdaptiveRuntime);
    let project = "agent:missing:work-result".to_string();
    let session = runtime.sessions.start_session(
        Some(project.clone()),
        Some("Work Result wrapper suppression".to_string()),
    );
    let before = runtime.sessions.summary(&session.session_id, None).unwrap();

    let outcome = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(5122)),
            mcp_2026_ui_params(json!({
                "name": "work_result_state",
                "arguments": {
                    "project": project,
                    "session_id": session.session_id,
                    "recording_session_id": session.session_id
                }
            })),
        ),
        None,
        true,
    )
    .await;
    assert!(matches!(
        outcome,
        McpOutcome::Ok(_) | McpOutcome::BadRequest(_)
    ));

    let after = runtime.sessions.summary(&session.session_id, None).unwrap();
    assert_eq!(after.events_total, before.events_total);
    assert_eq!(after.events.len(), before.events.len());
    assert_eq!(after.updated_at, before.updated_at);
}

#[test]
fn work_result_html_is_bounded_display_only_manual_refresh_ui() {
    for required in [
        "work_result_state",
        "ui/notifications/tool-input",
        "ui/notifications/tool-result",
        "id=\"refresh\"",
        "Refreshing…",
        "ui/resource-teardown",
        "state_version",
        "pagehide",
        "beforeunload",
        "WebCodex Work",
    ] {
        assert!(
            MCP_WORK_RESULT_APP_HTML.contains(required),
            "missing {required}"
        );
    }
    for forbidden in [
        "setInterval",
        "clearInterval",
        "visibilitychange",
        "POLL_MS",
        "pollTimer",
        "localStorage",
        "indexedDB",
        "fetch(",
        "WebSocket",
        "ui/message",
        "job_id",
        "continuationToken",
        "authority_fingerprint",
    ] {
        assert!(
            !MCP_WORK_RESULT_APP_HTML.contains(forbidden),
            "Work Result App contains forbidden marker {forbidden}"
        );
    }
}
