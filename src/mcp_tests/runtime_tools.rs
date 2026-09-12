use super::*;

// =========================================================================
// runtime_status via MCP tools/list and tools/call
// =========================================================================

// Full Operator is a compatibility surface whose unset compact-schema policy
// remains full, including outputSchema. Serialize it with process-global env
// tests so a concurrent override cannot change the observed contract.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn mcp_tools_list_exposes_canonical_coding_bootstrap_and_runtime_status_ux_flags() {
    let mut env = crate::test_support::TestEnvGuard::new();
    env.remove("WEBCODEX_MCP_COMPACT_SCHEMAS");
    let runtime = test_runtime_with_surface(ModelSurface::FullOperatorRuntime);
    let outcome = handle_mcp_request(
        &runtime,
        rpc("tools/list", Some(Value::from(10)), json!({})),
        None,
    )
    .await;
    let value = match outcome {
        McpOutcome::Ok(v) => v,
        other => panic!("expected Ok, got {:?}", other),
    };
    let tools = value["result"]["tools"].as_array().unwrap();
    let tool = |name: &str| {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing MCP tool {name}"))
    };
    assert!(
        tools.iter().all(|tool| tool["name"] != "start_coding_task"),
        "retired start_coding_task must stay out of MCP tools/list"
    );
    let description = tool("work_on_project")["description"].as_str().unwrap();
    assert!(description.contains("Canonical bootstrap"), "{description}");
    assert!(
        description.contains("ordinary coding/review"),
        "{description}"
    );
    assert!(description.contains("mode=worktree"), "{description}");
    assert!(description.contains("exact Git base"), "{description}");
    assert!(
        description.contains("fresh Workflow Session"),
        "{description}"
    );
    assert!(description.contains("exact resume"), "{description}");
    assert!(
        description.contains("current model context"),
        "{description}"
    );
    assert!(description.contains("Skills"), "{description}");
    assert!(description.contains("Plugin"), "{description}");
    assert!(
        description.contains("without bypassing Project authority"),
        "{description}"
    );

    let work_schema = &tool("work_on_project")["inputSchema"];
    assert!(
        work_schema["properties"]["path"].get("pattern").is_none(),
        "work_on_project path schema must not encode Control-host POSIX path semantics"
    );
    let work_props = work_schema["properties"]
        .as_object()
        .expect("work_on_project MCP properties");
    assert!(
        !work_props.contains_key("role"),
        "work_on_project must not grow a role wire field"
    );
    for field in [
        "project",
        "client_id",
        "path",
        "mode",
        "base_ref",
        "instruction",
        "include_project_instructions",
        "include_workflow_guidance",
        "include_extension_catalog",
        "session_id",
    ] {
        assert!(work_props.contains_key(field), "MCP schema missing {field}");
    }
    assert_eq!(work_props["include_project_instructions"]["default"], true);
    assert_eq!(work_props["include_workflow_guidance"]["default"], true);
    assert_eq!(work_props["include_extension_catalog"]["default"], true);
    assert_eq!(work_props["mode"]["enum"], json!(["checkout", "worktree"]));
    assert_eq!(work_props["mode"]["default"], "checkout");
    assert_eq!(work_schema["required"], json!(["instruction"]));
    assert_eq!(work_schema["additionalProperties"], false);
    for keyword in [
        "oneOf",
        "anyOf",
        "allOf",
        "not",
        "dependentRequired",
        "if",
        "then",
        "else",
    ] {
        assert!(
            work_schema.get(keyword).is_none(),
            "work_on_project MCP schema must not expose top-level {keyword}"
        );
    }

    let finish_props = tool("finish_coding_task")["inputSchema"]["properties"]
        .as_object()
        .expect("finish_coding_task inputSchema properties");
    assert!(
        finish_props.contains_key("include_workspace"),
        "MCP finish_coding_task schema should expose include_workspace"
    );
    let finish_required = tool("finish_coding_task")["inputSchema"]["required"]
        .as_array()
        .expect("finish_coding_task required fields");
    assert!(
        !finish_required
            .iter()
            .any(|field| field.as_str() == Some("include_workspace")),
        "include_workspace must not be required in MCP schema"
    );

    let update = tool("update_session_context");
    assert_eq!(
        update["inputSchema"]["required"],
        json!(["project", "session_id", "execution_context"])
    );
    assert_eq!(update["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        update["inputSchema"]["properties"]["execution_context"]["additionalProperties"],
        false
    );

    let runtime_props = tool("runtime_status")["inputSchema"]["properties"]
        .as_object()
        .expect("runtime_status inputSchema properties");
    for field in ["compact", "summary_only"] {
        assert!(
            runtime_props.contains_key(field),
            "MCP runtime_status schema should expose {field}"
        );
        assert_eq!(runtime_props[field]["type"], "boolean");
    }

    let overview = tool("project_overview");
    let overview_props = overview["inputSchema"]["properties"]
        .as_object()
        .expect("project_overview inputSchema properties");
    for field in ["project", "path", "max_depth", "limit"] {
        assert!(
            overview_props.contains_key(field),
            "MCP project_overview schema should expose {field}"
        );
    }
    let overview_output = overview["outputSchema"]["properties"]["output"]["properties"]
        .as_object()
        .expect("project_overview outputSchema properties");
    for field in ["project_types", "key_files", "top_level", "scan"] {
        assert!(
            overview_output.contains_key(field),
            "MCP project_overview output schema should expose {field}"
        );
    }
}

#[tokio::test]
async fn mcp_tools_call_runtime_status_returns_content() {
    // runtime_status is not part of the local_coding surface; select the full
    // operator surface so the call reaches dispatch.
    let runtime = test_runtime_with_surface(ModelSurface::FullOperatorRuntime);
    let outcome = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(Value::from(11)),
            json!({"name": "runtime_status", "arguments": {}}),
        ),
        None,
    )
    .await;
    let value = match outcome {
        McpOutcome::Ok(v) => v,
        other => panic!("expected Ok, got {:?}", other),
    };
    assert_eq!(value["id"], 11);
    // content blocks
    assert!(value["result"]["content"].is_array());
    assert_eq!(value["result"]["content"][0]["type"], "text");
    assert_eq!(
        value["result"]["content"][0]["text"],
        "WebCodex tool completed successfully."
    );
    // structuredContent carries the ToolResult shape exactly once; content.text
    // must not serialize the structured payload again.
    assert!(value["result"]["structuredContent"].is_object());
    assert_eq!(value["result"]["structuredContent"]["success"], true);
    let out = &value["result"]["structuredContent"]["output"];
    assert_eq!(out["service"], "webcodex");
    assert_eq!(out["version"], env!("CARGO_PKG_VERSION"));
    assert!(!value["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains(env!("CARGO_PKG_VERSION")));
    // runtime_status never errors on a failed-projects runtime — it
    // reports configured=false instead.
    assert_eq!(value["result"]["isError"], false);
}

#[tokio::test]
async fn mcp_tools_call_show_changes_returns_structured_tool_error() {
    let runtime = test_runtime();
    let outcome = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(Value::from(14)),
            json!({
                "name": "show_changes",
                "arguments": {"project": "agent:nope:nope"}
            }),
        ),
        None,
    )
    .await;
    let value = match outcome {
        McpOutcome::Ok(v) => v,
        other => panic!("expected Ok, got {:?}", other),
    };
    assert_eq!(value["id"], 14);
    assert_eq!(value["result"]["isError"], true);
    assert_eq!(value["result"]["structuredContent"]["success"], false);
    assert_eq!(
        value["result"]["structuredContent"]["output"]["error_kind"],
        "unknown_project"
    );
}
