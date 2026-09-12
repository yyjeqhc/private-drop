use super::*;
use std::sync::Arc;

fn tool<'a>(payload: &'a Value, name: &str) -> Option<&'a Value> {
    payload["tools"]
        .as_array()?
        .iter()
        .find(|tool| tool["name"] == name)
}

fn goal_auth(username: &str) -> crate::auth::AuthContext {
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

fn goal_runtime(
    surface: ModelSurface,
) -> (tempfile::TempDir, Arc<crate::db::Database>, ToolRuntime) {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(crate::db::Database::open(&temp.path().join("goal-plan.db")).unwrap());
    let runtime = ToolRuntime::new_for_tests()
        .with_model_surface(surface)
        .with_communication_database(db.clone());
    (temp, db, runtime)
}

fn create_goal(runtime: &ToolRuntime, auth: &crate::auth::AuthContext, key: &str) -> String {
    let created = runtime.create_goal(
        Some(auth),
        "Ship Goal Plan presentation".to_string(),
        "Expose one bounded read-only durable Goal card with exact-state polling.".to_string(),
        key.to_string(),
    );
    assert!(created.success, "{:?}", created.output);
    created.output["goal"]["summary"]["goal_id"]
        .as_str()
        .unwrap()
        .to_string()
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
async fn goal_plan_app_descriptor_is_sparse_app_only_resource_backed_and_adaptive_direct() {
    let (_temp, _db, adaptive) = goal_runtime(ModelSurface::AdaptiveRuntime);
    let auth = goal_auth("goal-plan-descriptor");

    let ui = handle_with_server_apps_enabled(
        &adaptive,
        rpc(
            "tools/list",
            Some(json!(4101)),
            mcp_2026_ui_params(json!({})),
        ),
        Some(&auth),
        true,
    )
    .await;
    let ui = match ui {
        McpOutcome::Ok(value) => value,
        other => panic!("expected UI-capable adaptive tools/list: {other:?}"),
    };
    let present = tool(&ui["result"], "present_goal_plan").expect("adaptive present_goal_plan");
    assert_eq!(
        present.pointer("/_meta/ui/resourceUri"),
        Some(&json!(MCP_GOAL_PLAN_UI_RESOURCE_URI))
    );
    assert!(present.pointer("/_meta/ui/visibility").is_none());
    let state = tool(&ui["result"], "goal_plan_state").expect("app-only goal_plan_state");
    assert_eq!(state.pointer("/_meta/ui/visibility"), Some(&json!(["app"])));
    assert!(state.pointer("/_meta/ui/resourceUri").is_none());
    assert_eq!(state["inputSchema"]["required"], json!(["goal_id"]));
    assert_eq!(
        state["inputSchema"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["goal_id".to_string()]
    );

    let plain = handle_with_server_apps_enabled(
        &adaptive,
        rpc("tools/list", Some(json!(4102)), mcp_2026_params(json!({}))),
        Some(&auth),
        true,
    )
    .await;
    let McpOutcome::Ok(plain) = plain else {
        panic!("expected ordinary adaptive tools/list");
    };
    assert!(tool(&plain["result"], "present_goal_plan").is_some());
    assert!(tool(&plain["result"], "present_goal_plan")
        .unwrap()
        .pointer("/_meta/ui/resourceUri")
        .is_none());
    assert!(tool(&plain["result"], "goal_plan_state").is_none());

    let disabled_ui = handle_with_server_apps_enabled(
        &adaptive,
        rpc(
            "tools/list",
            Some(json!(4106)),
            mcp_2026_ui_params(json!({})),
        ),
        Some(&auth),
        false,
    )
    .await;
    let McpOutcome::Ok(disabled_ui) = disabled_ui else {
        panic!("expected tools/list with Server Apps disabled");
    };
    let disabled_present = tool(&disabled_ui["result"], "present_goal_plan")
        .expect("present_goal_plan remains a normal read tool");
    assert!(disabled_present.pointer("/_meta/ui/resourceUri").is_none());
    assert!(tool(&disabled_ui["result"], "goal_plan_state").is_none());

    let (_full_temp, _full_db, full) = goal_runtime(ModelSurface::FullOperatorRuntime);
    let full_ui = handle_with_server_apps_enabled(
        &full,
        rpc(
            "tools/list",
            Some(json!(4103)),
            mcp_2026_ui_params(json!({})),
        ),
        Some(&auth),
        true,
    )
    .await;
    let McpOutcome::Ok(full_ui) = full_ui else {
        panic!("expected UI-capable Full Operator tools/list");
    };
    for name in [
        "create_goal",
        "get_goal",
        "list_goals",
        "update_goal",
        "associate_goal_agent_task",
        "associate_goal_workflow_session",
        "work_on_project",
        "run_process",
        "run_shell",
        "finish_coding_task",
    ] {
        let descriptor = tool(&full_ui["result"], name).unwrap_or_else(|| panic!("missing {name}"));
        assert_ne!(
            descriptor
                .pointer("/_meta/ui/resourceUri")
                .and_then(Value::as_str),
            Some(MCP_GOAL_PLAN_UI_RESOURCE_URI),
            "{name} must not create another Goal Plan card"
        );
    }

    let resources = handle_with_server_apps_enabled(
        &full,
        rpc(
            "resources/list",
            Some(json!(4104)),
            mcp_2026_ui_params(json!({})),
        ),
        Some(&auth),
        true,
    )
    .await;
    let McpOutcome::Ok(resources) = resources else {
        panic!("expected Goal Plan resources/list");
    };
    let resource = resources["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|resource| resource["uri"] == MCP_GOAL_PLAN_UI_RESOURCE_URI)
        .expect("Goal Plan App resource");
    assert_eq!(resource["mimeType"], MCP_UI_RESOURCE_MIME_TYPE);
    assert_eq!(
        resource["_meta"]["ui"]["csp"],
        json!({"connectDomains": [], "resourceDomains": []})
    );

    let read = handle_with_server_apps_enabled(
        &full,
        rpc(
            "resources/read",
            Some(json!(4105)),
            mcp_2026_ui_params(json!({"uri": MCP_GOAL_PLAN_UI_RESOURCE_URI})),
        ),
        Some(&auth),
        true,
    )
    .await;
    let McpOutcome::Ok(read) = read else {
        panic!("expected Goal Plan resource read");
    };
    assert_eq!(
        read["result"]["contents"][0]["uri"],
        MCP_GOAL_PLAN_UI_RESOURCE_URI
    );
    assert_eq!(
        read["result"]["contents"][0]["text"],
        MCP_GOAL_PLAN_APP_HTML
    );
    for required in [
        "goal_plan_state",
        "visibilitychange",
        "ui/resource-teardown",
        "lastRevision",
        "setInterval",
        "pagehide",
    ] {
        assert!(
            MCP_GOAL_PLAN_APP_HTML.contains(required),
            "missing App behavior {required}"
        );
    }
    for forbidden in [
        "localStorage",
        "ui/message",
        "consume_token",
        "wake_token",
        "attempt_fence",
        "authority_fingerprint",
    ] {
        assert!(
            !MCP_GOAL_PLAN_APP_HTML.contains(forbidden),
            "Goal Plan App contains forbidden G3/private marker {forbidden}"
        );
    }
}

#[tokio::test]
async fn goal_plan_poll_reads_authoritative_revision_without_ui_request_identity_or_mutation() {
    let (_temp, _db, runtime) = goal_runtime(ModelSurface::AdaptiveRuntime);
    let bob = goal_auth("goal-plan-bob");
    let alice = goal_auth("goal-plan-alice");
    let goal_id = create_goal(&runtime, &bob, "goal-plan-bob-create");

    let present = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(4201)),
            mcp_2026_ui_params(json!({
                "name": "present_goal_plan",
                "arguments": {"goal_id": goal_id}
            })),
        ),
        Some(&bob),
        true,
    )
    .await;
    let McpOutcome::Ok(present) = present else {
        panic!("present_goal_plan failed");
    };
    assert_eq!(present["result"]["structuredContent"]["success"], true);
    assert_eq!(
        present["result"]["structuredContent"]["output"]["goal_plan"]["goal_id"],
        goal_id
    );
    assert_eq!(
        present["result"]["structuredContent"]["output"]["goal_plan"]["revision"],
        1
    );

    // App polling must not rely on the initiating tools/list/call carrying UI capability
    // metadata. Exact Goal identity plus the caller's normal Goal authority is sufficient.
    let poll = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(4202)),
            mcp_2026_params(json!({
                "name": "goal_plan_state",
                "arguments": {"goal_id": goal_id}
            })),
        ),
        Some(&bob),
        true,
    )
    .await;
    let McpOutcome::Ok(poll) = poll else {
        panic!("app-only Goal polling failed");
    };
    assert_eq!(poll["result"]["structuredContent"]["success"], true);
    assert_eq!(
        poll["result"]["structuredContent"]["output"]["goal_plan"]["revision"],
        1
    );
    assert_eq!(
        runtime.get_goal(Some(&bob), goal_id.clone()).output["goal"]["summary"]["revision"],
        1
    );

    let update = runtime.update_goal(
        Some(&bob),
        goal_id.clone(),
        1,
        None,
        Some("Revision two objective".to_string()),
        None,
        None,
        "goal-plan-revision-two".to_string(),
    );
    assert!(update.success, "{:?}", update.output);
    let poll2 = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(4203)),
            mcp_2026_params(json!({
                "name": "goal_plan_state",
                "arguments": {"goal_id": goal_id}
            })),
        ),
        Some(&bob),
        true,
    )
    .await;
    let McpOutcome::Ok(poll2) = poll2 else {
        panic!("second app-only Goal polling failed");
    };
    assert_eq!(
        poll2["result"]["structuredContent"]["output"]["goal_plan"]["revision"],
        2
    );
    assert_eq!(
        poll2["result"]["structuredContent"]["output"]["goal_plan"]["objective"],
        "Revision two objective"
    );
    assert_eq!(
        runtime.get_goal(Some(&bob), goal_id.clone()).output["goal"]["summary"]["revision"],
        2
    );

    let foreign = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(4204)),
            mcp_2026_params(json!({
                "name": "goal_plan_state",
                "arguments": {"goal_id": goal_id}
            })),
        ),
        Some(&alice),
        true,
    )
    .await;
    let McpOutcome::Ok(foreign) = foreign else {
        panic!("foreign exact lookup must return an existence-hidden tool result");
    };
    assert_eq!(foreign["result"]["structuredContent"]["success"], false);
    assert_eq!(
        foreign["result"]["structuredContent"]["output"]["error_kind"],
        "goal_not_found"
    );

    let disabled = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(4205)),
            mcp_2026_params(json!({
                "name": "goal_plan_state",
                "arguments": {"goal_id": goal_id}
            })),
        ),
        Some(&bob),
        false,
    )
    .await;
    assert!(matches!(disabled, McpOutcome::BadRequest(_)));
}
