use super::*;

fn model_surface_direct_auth() -> crate::auth::AuthContext {
    // Exercise the declared direct surface without also admitting the separate
    // mcp_tool/ssh_resource gateways. Plugin scopes are explicit because
    // plugin_tool is a scope-gated canonical direct gateway after #317.
    let mut auth = crate::auth::shared_key_context("model-surface-direct-test");
    auth.scopes.extend([
        crate::auth::SCOPE_PLUGIN_INSPECT.to_string(),
        crate::auth::SCOPE_PLUGIN_INVOKE.to_string(),
        crate::auth::SCOPE_PLUGIN_MANAGE.to_string(),
    ]);
    auth
}

// =========================================================================
// local_coding model surface
// =========================================================================

#[tokio::test]
async fn local_coding_tools_list_returns_exact_ordered_surface() {
    // Explicit local_coding surface; names are compact-invariant, so no env
    // or lock is needed.
    let runtime = test_runtime_with_surface(ModelSurface::LocalCoding);
    let auth = model_surface_direct_auth();
    let outcome = handle_mcp_request(
        &runtime,
        rpc("tools/list", Some(Value::from(60)), json!({})),
        Some(&auth),
    )
    .await;
    let value = match outcome {
        McpOutcome::Ok(v) => v,
        other => panic!("expected Ok, got {:?}", other),
    };
    let tools = value["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        crate::tool_runtime::tool_definition::LOCAL_CODING_TOOL_NAMES,
        "local_coding tools/list must be the exact ordered surface"
    );
    // Focused, not the full runtime surface.
    assert!(
        names.len() < registered_tool_specs().len(),
        "local_coding must expose fewer tools than the full runtime"
    );
    for required in [
        "work_on_project",
        "read_file",
        "read_files",
        "search_project_texts",
        "get_session_assignment",
        "complete_session_message",
        "apply_text_edits",
        "apply_patch",
        "go_test",
        "finish_coding_task",
    ] {
        assert!(names.contains(&required), "missing {required}: {names:?}");
    }
    for forbidden in [
        "start_coding_task",
        "register_project",
        "unregister_project",
        "create_project",
        "start_session",
        "current_session",
        "open_session_shell",
        "session_shell_exec",
        "close_session_shell",
        "runtime_status",
        "tool_manifest",
        "workspace_checkpoint_create",
        "delete_project_files",
        "git_restore_paths",
        "discard_untracked",
    ] {
        assert!(
            !names.contains(&forbidden),
            "local_coding must not expose {forbidden}: {names:?}"
        );
    }

    let work = tools
        .iter()
        .find(|tool| tool["name"] == "work_on_project")
        .expect("local_coding work_on_project");
    let schema = &work["inputSchema"];
    let props = schema["properties"].as_object().unwrap();
    for field in [
        "project",
        "client_id",
        "path",
        "instruction",
        "include_project_instructions",
        "include_workflow_guidance",
        "session_id",
    ] {
        assert!(props.contains_key(field), "local_coding missing {field}");
    }
    assert_eq!(props["include_project_instructions"]["default"], true);
    assert_eq!(props["include_workflow_guidance"]["default"], true);
    assert_eq!(schema["required"], json!(["instruction"]));
    assert_eq!(schema["additionalProperties"], false);
    assert!(schema.get("oneOf").is_none());
    assert!(schema.get("not").is_none());
}

#[tokio::test]
async fn apply_text_edits_discriminated_schema_reaches_full_and_local_coding_mcp_surfaces() {
    let expected = registered_tool_specs()
        .into_iter()
        .find(|spec| spec.name == "apply_text_edits")
        .expect("apply_text_edits ToolSpec")
        .input_schema;
    assert_eq!(
        expected["properties"]["changes"]["items"]["oneOf"]
            .as_array()
            .unwrap()
            .len(),
        4
    );

    for surface in [
        ModelSurface::FullOperatorRuntime,
        ModelSurface::LocalCoding,
        ModelSurface::AdaptiveRuntime,
    ] {
        let runtime = test_runtime_with_surface(surface);
        let outcome = handle_mcp_request(
            &runtime,
            rpc("tools/list", Some(Value::from(601)), json!({})),
            None,
        )
        .await;
        let value = match outcome {
            McpOutcome::Ok(value) => value,
            other => panic!("expected tools/list success for {surface:?}, got {other:?}"),
        };
        let schema = &value["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "apply_text_edits")
            .unwrap_or_else(|| panic!("missing apply_text_edits on {surface:?}"))["inputSchema"];
        assert_eq!(schema, &expected, "schema drift on {surface:?}");
    }
}

#[tokio::test]
async fn apply_patch_schema_reaches_full_local_and_adaptive_surfaces() {
    let expected = registered_tool_specs()
        .into_iter()
        .find(|spec| spec.name == "apply_patch")
        .expect("apply_patch ToolSpec")
        .input_schema;
    assert!(expected["properties"].get("patch").is_some());
    assert_eq!(expected["properties"]["dry_run"]["default"], false);

    for surface in [
        ModelSurface::FullOperatorRuntime,
        ModelSurface::LocalCoding,
        ModelSurface::AdaptiveRuntime,
    ] {
        let runtime = test_runtime_with_surface(surface);
        let outcome = handle_mcp_request(
            &runtime,
            rpc("tools/list", Some(Value::from(602)), json!({})),
            None,
        )
        .await;
        let value = match outcome {
            McpOutcome::Ok(value) => value,
            other => panic!("expected tools/list success for {surface:?}, got {other:?}"),
        };
        let schema = &value["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "apply_patch")
            .unwrap_or_else(|| panic!("missing apply_patch on {surface:?}"))["inputSchema"];
        assert_eq!(schema, &expected, "schema drift on {surface:?}");
    }
}

#[tokio::test]
async fn local_coding_default_initialize_and_discovery_report_local_coding() {
    // Preserve the unset-env integration path, but confine process env state
    // to synchronous runtime construction.
    let runtime = test_runtime_from_model_surface_env(None);
    let outcome = handle_mcp_request(
        &runtime,
        rpc("initialize", Some(Value::from(61)), json!({})),
        None,
    )
    .await;
    let value = match outcome {
        McpOutcome::Ok(v) => v,
        other => panic!("expected Ok, got {:?}", other),
    };
    assert_eq!(
        value["result"]["serverInfo"]["runtimeExposure"],
        crate::model_surface::MODEL_SURFACE_LOCAL_CODING
    );
}

#[tokio::test]
async fn local_coding_rejects_non_surface_tools_at_mcp_boundary() {
    // Explicit local_coding surface: no env or lock needed.
    let runtime = test_runtime_with_surface(ModelSurface::LocalCoding);
    for denied in [
        "start_coding_task",
        "register_project",
        "unregister_project",
        "create_project",
        "start_session",
        "open_session_shell",
        "runtime_status",
        "tool_manifest",
        "workspace_checkpoint_create",
    ] {
        let outcome = handle_mcp_request(
            &runtime,
            rpc(
                "tools/call",
                Some(json!(70)),
                json!({"name": denied, "arguments": {}}),
            ),
            None,
        )
        .await;
        match outcome {
            McpOutcome::BadRequest(value) => {
                assert_eq!(value["error"]["code"], -32602);
                assert!(
                    value["error"]["message"].as_str().unwrap().contains(denied),
                    "denial message must name the tool: {:?}",
                    value
                );
                assert!(
                    value["error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("local_coding"),
                    "denial message must name the surface: {:?}",
                    value
                );
            }
            other => panic!("{denied} must be rejected, got {:?}", other),
        }
    }
}

#[tokio::test]
async fn local_coding_allows_surface_tools_to_dispatch() {
    // Explicit local_coding surface: no env or lock needed.
    let runtime = test_runtime_with_surface(ModelSurface::LocalCoding);
    // list_projects and work_on_project resolve to the runtime registry; they
    // must reach dispatch (not be rejected at the MCP boundary).
    let outcome = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(71)),
            json!({"name": "list_projects", "arguments": {}}),
        ),
        None,
    )
    .await;
    match outcome {
        McpOutcome::Ok(value) => {
            assert_eq!(value["result"]["structuredContent"]["success"], true);
        }
        other => panic!("list_projects must dispatch, got {:?}", other),
    }
}

// =========================================================================
// adaptive_runtime model surface
// =========================================================================

#[tokio::test]
async fn adaptive_runtime_tools_list_is_small_core_plus_gateway() {
    let runtime = test_runtime_with_surface(ModelSurface::AdaptiveRuntime);
    let auth = model_surface_direct_auth();
    let outcome = handle_mcp_request(
        &runtime,
        rpc(
            "tools/list",
            Some(Value::from(720)),
            mcp_2026_params(json!({})),
        ),
        Some(&auth),
    )
    .await;
    let McpOutcome::Ok(value) = outcome else {
        panic!("adaptive tools/list must succeed");
    };
    let tools = value["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    let direct_names = crate::model_surface::adaptive_runtime_direct_tool_specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    assert_eq!(
        names.len(),
        direct_names.len() + 1,
        "adaptive surface should expose only the definition-derived direct set plus one gateway"
    );
    assert_eq!(
        &names[..direct_names.len()],
        direct_names.iter().map(String::as_str).collect::<Vec<_>>()
    );
    let serialized_tools_bytes = serde_json::to_vec(tools).unwrap().len();
    // P5 admits directly actionable recovery targets and #317 adds the stable
    // scope-gated plugin_tool gateway to the declared direct set. Measured
    // post-#317 baseline is ~547,411 bytes (~534.6 KiB). Keep roughly the same
    // ~13% schema-growth headroom without turning the exact tool count into an
    // architectural lock.
    const MAX_ADAPTIVE_RUNTIME_TOOLS_LIST_BYTES: usize = 608 * 1024;
    assert!(
        serialized_tools_bytes <= MAX_ADAPTIVE_RUNTIME_TOOLS_LIST_BYTES,
        "adaptive tools/list schema cost {serialized_tools_bytes} exceeded {MAX_ADAPTIVE_RUNTIME_TOOLS_LIST_BYTES} bytes"
    );
    assert_eq!(
        names.last().copied(),
        Some(crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME)
    );
    for long_tail in [
        "list_tools",
        "list_projects",
        "project_overview",
        "read_file",
        "run_script",
        "run_shell",
        "validation_summary",
        "git_status",
        "goto_definition",
        "computer_list_windows",
        "post_session_message",
        "coding_agent_start",
        "artifact_upload_begin",
    ] {
        assert!(
            !names.contains(&long_tail),
            "{long_tail} must stay behind the adaptive gateway"
        );
    }

    let gateway = tools
        .iter()
        .find(|tool| tool["name"] == crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME)
        .expect("adaptive gateway");
    let properties = gateway["inputSchema"]["properties"].as_object().unwrap();
    for field in [
        "tool",
        "arguments",
        "recording_session_id",
        "ack_session_message_ids",
        "session_message_resolution",
        "context_request",
        "ack_session_context_revision",
    ] {
        assert!(
            properties.contains_key(field),
            "adaptive gateway missing stateless wrapper field {field}"
        );
    }
}

#[tokio::test]
async fn adaptive_runtime_requires_gateway_for_long_tail_and_preserves_dispatch() {
    let runtime = test_runtime_with_surface(ModelSurface::AdaptiveRuntime);
    let direct = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(7201)),
            mcp_2026_params(json!({
                "name": "runtime_status",
                "arguments": {"summary_only": true}
            })),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(value) = direct else {
        panic!("adaptive direct runtime_status must dispatch directly");
    };
    assert_eq!(value["result"]["structuredContent"]["success"], true);

    let direct = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(721)),
            mcp_2026_params(json!({
                "name": "list_tools",
                "arguments": {"summary_only": true, "limit": 1}
            })),
        ),
        None,
    )
    .await;
    match direct {
        McpOutcome::BadRequest(value) => {
            assert_eq!(value["error"]["code"], -32602);
            assert!(value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("adaptive_runtime"));
        }
        other => panic!("direct adaptive long-tail call must be rejected, got {other:?}"),
    }

    let gateway = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(722)),
            mcp_2026_params(json!({
                "name": crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
                "arguments": {
                    "tool": "list_tools",
                    "arguments": {"summary_only": true, "limit": 1}
                }
            })),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(value) = gateway else {
        panic!("adaptive gateway must dispatch an allowed long-tail tool");
    };
    assert_eq!(value["result"]["structuredContent"]["success"], true);
    assert_eq!(
        value["result"]["structuredContent"]["output"]["returned_count"],
        1
    );

    let admin = crate::auth::AuthContext {
        role: Some("admin".to_string()),
        scopes: vec![crate::auth::SCOPE_ADMIN.to_string()],
        is_bootstrap: true,
        ..crate::auth::AuthContext::new(crate::auth::AuthKind::Bootstrap)
    };
    let trace_gateway = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(723)),
            mcp_2026_params(json!({
                "name": crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
                "arguments": {
                    "tool": "read_tool_trace",
                    "arguments": {"trace_ref": "not-a-trace-ref"}
                }
            })),
        ),
        Some(&admin),
    )
    .await;
    let McpOutcome::Ok(value) = trace_gateway else {
        panic!("adaptive gateway must admit the admin-only trace reader target");
    };
    assert_eq!(value["result"]["structuredContent"]["success"], false);
    assert!(matches!(
        value["result"]["structuredContent"]["output"]["error_kind"].as_str(),
        Some("trace_mode_not_full" | "invalid_trace_ref")
    ));
}

#[tokio::test]
async fn adaptive_runtime_gateway_uses_long_tail_target_checkpoint_policy_once() {
    let runtime = test_runtime_with_surface(ModelSurface::AdaptiveRuntime);
    let gateway_session = runtime.sessions.start_session(
        Some("missing-project".to_string()),
        Some("gateway checkpoint parity".to_string()),
    );

    let gateway_read = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(7221)),
            mcp_2026_params(json!({
                "name": crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
                "arguments": {
                    "tool": "read_file",
                    "arguments": {
                        "project": "missing-project",
                        "path": "src/lib.rs"
                    },
                    "recording_session_id": gateway_session.session_id,
                    "ack_session_context_revision": 999
                }
            })),
        ),
        None,
    )
    .await;
    assert!(matches!(gateway_read, McpOutcome::Ok(_)));
    assert_eq!(
        runtime
            .sessions
            .context_revision(&gateway_session.session_id),
        Some(0),
        "gateway read must inherit the long-tail target no-checkpoint policy"
    );

    let gateway_script = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(7222)),
            mcp_2026_params(json!({
                "name": crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
                "arguments": {
                    "tool": "run_script",
                    "arguments": {
                        "project": "missing-project",
                        "language": "bash",
                        "script": "exit 0"
                    },
                    "recording_session_id": gateway_session.session_id,
                    "ack_session_context_revision": 0
                }
            })),
        ),
        None,
    )
    .await;
    assert!(matches!(gateway_script, McpOutcome::Ok(_)));
    assert_eq!(
        runtime
            .sessions
            .context_revision(&gateway_session.session_id),
        Some(1),
        "one checkpoint-capable gateway invocation must allocate exactly one revision"
    );
}

#[tokio::test]
async fn adaptive_runtime_gateway_returns_exact_route_recovery_without_dispatching_direct_tools() {
    let runtime = test_runtime_with_surface(ModelSurface::AdaptiveRuntime);
    for target in ["runtime_status", "read_files", "work_on_project"] {
        let outcome = handle_mcp_request(
            &runtime,
            rpc(
                "tools/call",
                Some(json!(723)),
                mcp_2026_params(json!({
                    "name": crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
                    "arguments": {"tool": target, "arguments": {}}
                })),
            ),
            None,
        )
        .await;
        let McpOutcome::Ok(value) = outcome else {
            panic!("known direct target {target} should return structured recovery");
        };
        let structured = &value["result"]["structuredContent"];
        assert_eq!(structured["success"], false);
        let output = &structured["output"];
        assert_eq!(output["error_kind"], "wrong_invocation_route");
        assert_eq!(output["execution_state"], "not_started");
        assert_eq!(output["state_changed"], false);
        assert_eq!(output["target_tool"], target);
        assert_eq!(output["correct_route"]["mode"], "direct");
        assert_eq!(output["recovery"]["tool"], target);
        assert_eq!(output["recovery"]["route"]["mode"], "direct");
        assert_eq!(output["recovery_kind"], "fix_input");
    }

    let unknown = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(7231)),
            mcp_2026_params(json!({
                "name": crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
                "arguments": {"tool": "not_a_real_webcodex_tool", "arguments": {}}
            })),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(value) = unknown else {
        panic!("unknown gateway target should return a structured unknown-tool result");
    };
    let output = &value["result"]["structuredContent"]["output"];
    assert_eq!(output["error_kind"], "unknown_tool");
    assert_eq!(output["execution_state"], "not_started");
    assert_eq!(output["state_changed"], false);
    assert!(output.get("correct_route").is_none());

    let recursive = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(7232)),
            mcp_2026_params(json!({
                "name": crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
                "arguments": {
                    "tool": crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
                    "arguments": {}
                }
            })),
        ),
        None,
    )
    .await;
    let McpOutcome::BadRequest(value) = recursive else {
        panic!("recursive adaptive gateway target must remain invalid arguments");
    };
    assert_eq!(value["error"]["code"], -32602);
}

#[tokio::test]
async fn adaptive_runtime_gateway_route_classification_does_not_mask_target_scope_denials() {
    let runtime = test_runtime_with_surface(ModelSurface::AdaptiveRuntime);
    let mut auth = crate::auth::AuthContext::new(crate::auth::AuthKind::OAuth2Token);
    auth.scopes = vec![crate::auth::SCOPE_RUNTIME_READ.to_string()];

    let direct_target = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(7233)),
            mcp_2026_params(json!({
                "name": crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
                "arguments": {
                    "tool": "read_files",
                    "arguments": {"project": "missing-project", "items": [{"path": "src/lib.rs"}]}
                }
            })),
        ),
        Some(&auth),
    )
    .await;
    let McpOutcome::Forbidden {
        required_scope,
        body,
    } = direct_target
    else {
        panic!("wrong-route read_files must not mask its canonical scope denial");
    };
    assert_eq!(required_scope, Some(crate::auth::SCOPE_PROJECT_READ));
    assert!(body.to_string().contains(crate::auth::SCOPE_PROJECT_READ));
    assert!(!body.to_string().contains("wrong_invocation_route"));

    let outcome = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(7234)),
            mcp_2026_params(json!({
                "name": crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
                "arguments": {
                    "tool": "read_file",
                    "arguments": {"project": "missing-project", "path": "src/lib.rs"}
                }
            })),
        ),
        Some(&auth),
    )
    .await;
    let McpOutcome::Forbidden {
        required_scope,
        body,
    } = outcome
    else {
        panic!("gateway-routed read_file must retain its canonical scope denial");
    };
    assert_eq!(required_scope, Some(crate::auth::SCOPE_PROJECT_READ));
    assert!(body.to_string().contains(crate::auth::SCOPE_PROJECT_READ));
    assert!(!body.to_string().contains("wrong_invocation_route"));
}

#[tokio::test]
async fn adaptive_runtime_tool_manifest_exact_projection_is_sparse_and_routes_explicitly() {
    let runtime = test_runtime_with_surface(ModelSurface::AdaptiveRuntime);
    let canonical_direct = runtime
        .dispatch(crate::tool_runtime::ToolCall::ToolManifest {
            tool_name: Some("read_files".to_string()),
            category: None,
            intent: None,
            include_recommended_flows: true,
            include_risk_summary: true,
        })
        .await;
    let described = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(724)),
            mcp_2026_params(json!({
                "name": "tool_manifest",
                "arguments": {
                    "tool_name": "read_files"
                }
            })),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(value) = described else {
        panic!("adaptive tool_manifest exact contract must succeed");
    };
    let output = &value["result"]["structuredContent"]["output"];
    assert_eq!(output["name"], "read_files");
    assert!(output["description"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));
    assert_eq!(output["route"]["mode"], "direct");
    assert!(output["route"].get("via").is_none());
    assert_eq!(output["input_schema"]["type"], "object");
    assert!(output["input_schema"]["properties"]["items"].is_object());
    assert_eq!(output["effect"], "observe");
    assert!(output["authority"]["scopes"].is_array());
    assert!(output["annotations"].is_object());
    for redundant in [
        "tool_name",
        "contract",
        "tools",
        "count",
        "returned_count",
        "filtered_count",
        "categories",
        "available_intents",
        "gateway_tool",
    ] {
        assert!(
            output.get(redundant).is_none(),
            "exact sparse manifest retained redundant field {redundant}: {output}"
        );
    }
    let canonical_bytes = serde_json::to_vec(&canonical_direct).unwrap().len();
    let sparse_bytes = serde_json::to_vec(&value["result"]["structuredContent"])
        .unwrap()
        .len();
    eprintln!("tool_manifest_exact_bytes before={canonical_bytes} after={sparse_bytes}");
    assert!(
        sparse_bytes < canonical_bytes,
        "{canonical_bytes} -> {sparse_bytes}"
    );

    let gateway = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(7241)),
            mcp_2026_params(json!({
                "name": "tool_manifest",
                "arguments": {
                    "tool_name": "run_script",
                    "include_recommended_flows": false,
                    "include_risk_summary": false
                }
            })),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(value) = gateway else {
        panic!("adaptive tool_manifest gateway contract must succeed");
    };
    let gateway_output = &value["result"]["structuredContent"]["output"];
    assert_eq!(gateway_output["name"], "run_script");
    assert_eq!(gateway_output["route"]["mode"], "gateway");
    assert_eq!(
        gateway_output["route"]["via"],
        crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME
    );

    let listed = handle_mcp_request(
        &runtime,
        rpc("tools/list", Some(json!(725)), mcp_2026_params(json!({}))),
        None,
    )
    .await;
    let McpOutcome::Ok(listed) = listed else {
        panic!("adaptive tools/list must remain available");
    };
    assert!(!listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "run_script"));
}

#[tokio::test]
async fn adaptive_runtime_tool_manifest_filtered_projection_is_selection_focused() {
    let runtime = test_runtime_with_surface(ModelSurface::AdaptiveRuntime);
    let canonical = runtime
        .dispatch(crate::tool_runtime::ToolCall::ToolManifest {
            tool_name: None,
            category: None,
            intent: Some("exploration".to_string()),
            include_recommended_flows: true,
            include_risk_summary: true,
        })
        .await;
    let outcome = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(7242)),
            mcp_2026_params(json!({
                "name": "tool_manifest",
                "arguments": {"intent": "exploration"}
            })),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(value) = outcome else {
        panic!("intent-filtered tool_manifest must succeed");
    };
    let output = &value["result"]["structuredContent"]["output"];
    assert_eq!(output["intent"], "exploration");
    assert!(output.get("categories").is_none());
    assert!(output.get("available_intents").is_none());
    assert!(output.get("risk_summary").is_none());
    assert!(output.get("count").is_none());
    assert!(output.get("returned_count").is_none());
    assert!(output.get("filtered_count").is_none());
    assert!(output.get("truncated").is_none());
    let tools = output["tools"].as_array().expect("filtered sparse tools");
    assert_eq!(
        tools.len(),
        canonical.output["tools"].as_array().unwrap().len()
    );
    for tool in tools {
        assert!(tool["name"].is_string());
        let description = tool["description"].as_str().expect("selection description");
        assert!(!description.is_empty());
        assert!(description.chars().count() <= 180, "{description}");
        assert!(matches!(
            tool["route"]["mode"].as_str(),
            Some("direct" | "gateway")
        ));
        assert!(tool["requires_project"].is_boolean());
        assert!(tool["effect"].is_string());
        for verbose in [
            "accepted_flattened_args",
            "deprecated_or_unsupported_args",
            "provider",
            "approval",
            "idempotency",
            "path_hint",
            "shell_like",
            "authority",
            "availability",
            "gateway_tool",
        ] {
            assert!(tool.get(verbose).is_none(), "{verbose} leaked into {tool}");
        }
    }
    let canonical_names = canonical.output["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].clone())
        .collect::<Vec<_>>();
    let sparse_names = tools
        .iter()
        .map(|tool| tool["name"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        sparse_names, canonical_names,
        "intent ordering must stay deterministic"
    );
    let canonical_bytes = serde_json::to_vec(&canonical).unwrap().len();
    let sparse_bytes = serde_json::to_vec(&value["result"]["structuredContent"])
        .unwrap()
        .len();
    eprintln!("tool_manifest_exploration_bytes before={canonical_bytes} after={sparse_bytes}");
    assert!(
        sparse_bytes < canonical_bytes,
        "{canonical_bytes} -> {sparse_bytes}"
    );
}

#[tokio::test]
async fn adaptive_runtime_tool_manifest_unfiltered_keeps_global_category_discovery() {
    let runtime = test_runtime_with_surface(ModelSurface::AdaptiveRuntime);
    let outcome = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(7243)),
            mcp_2026_params(json!({"name": "tool_manifest", "arguments": {}})),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(value) = outcome else {
        panic!("unfiltered tool_manifest must succeed");
    };
    let output = &value["result"]["structuredContent"]["output"];
    assert!(output["tool_count"]
        .as_u64()
        .is_some_and(|count| count > 100));
    assert!(output["categories"]
        .as_object()
        .is_some_and(|categories| !categories.is_empty()));
    assert!(output["available_intents"]
        .as_array()
        .is_some_and(|intents| !intents.is_empty()));
    assert!(
        output.get("tools").is_none(),
        "unfiltered discovery should not duplicate all category names"
    );
}

#[tokio::test]
async fn full_operator_explicit_surface_lists_full_runtime_and_dispatches() {
    let runtime = test_runtime_with_surface(ModelSurface::FullOperatorRuntime);
    let listed = handle_mcp_request(
        &runtime,
        rpc(
            "tools/list",
            Some(Value::from(72)),
            mcp_2026_params(json!({})),
        ),
        None,
    )
    .await;
    let value = match listed {
        McpOutcome::Ok(v) => v,
        other => panic!("expected Ok, got {:?}", other),
    };
    let names: Vec<String> = value["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    let expected = mcp_tools_list_payload_with_features_for_auth(
        ModelSurface::FullOperatorRuntime,
        false,
        false,
        true,
        true,
        None,
    );
    let registry_names: Vec<String> = expected["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names, registry_names,
        "full operator lists the full runtime"
    );
    assert!(names.iter().any(|name| name == "read_files"));
    assert!(names.iter().any(|name| name == "search_project_texts"));

    let called = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(73)),
            json!({"name": "start_coding_task", "arguments": {}}),
        ),
        None,
    )
    .await;
    let McpOutcome::BadRequest(value) = called else {
        panic!("retired start_coding_task must fail closed");
    };
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains("no longer supported"), "{message}");
    assert!(message.contains("work_on_project"), "{message}");
}

#[tokio::test]
async fn full_operator_tools_list_projects_destructive_hints_for_non_additive_mutations() {
    let runtime = test_runtime_with_surface(ModelSurface::FullOperatorRuntime);
    let listed = handle_mcp_request(
        &runtime,
        rpc(
            "tools/list",
            Some(Value::from(721)),
            mcp_2026_params(json!({})),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(value) = listed else {
        panic!("full operator tools/list must succeed");
    };
    let tools = value["result"]["tools"].as_array().unwrap();

    for name in [
        "apply_patch",
        "apply_text_edits",
        "apply_unified_diff",
        "write_project_file",
        "workspace_checkpoint_restore",
        "save_project_artifact",
        "import_conversation_files_to_project",
        "artifact_upload_finish",
        "artifact_upload_abort",
        "assign_agent_task",
        "reconcile_agent_task_coding_run",
        "heartbeat_agent_task_attempt",
        "complete_agent_task_attempt",
        "update_agent_identity",
        "attach_agent_endpoint",
        "detach_agent_endpoint",
        "consume_agent_deliveries",
        "consume_agent_wake",
        "coding_agent_cancel",
        "computer_write_clipboard",
        "computer_pointer_click",
        "computer_control",
        "computer_key_input",
        "update_session_context",
        "close_session",
        "resolve_session_message",
        "complete_session_message",
        "cargo_fmt",
    ] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing {name} from full operator tools/list"));
        assert_eq!(
            tool["annotations"]["destructiveHint"], true,
            "{name} may replace, restore, delete, or discard existing state"
        );
    }

    for name in [
        "workspace_checkpoint_create",
        "artifact_upload_begin",
        "artifact_upload_chunk",
        "computer_save_snapshot",
        "start_agent_task_attempt",
        "create_conversation",
        "post_conversation_message",
        "post_session_message",
        "work_on_project",
        "cargo_check",
    ] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing {name} from full operator tools/list"));
        assert_eq!(
            tool["annotations"]["destructiveHint"], false,
            "{name} is intentionally additive-only"
        );
    }
}

#[tokio::test]
async fn explicit_local_coding_v1_selects_local_coding() {
    let runtime = test_runtime_from_model_surface_env(Some(
        crate::model_surface::MCP_MODEL_SURFACE_LOCAL_CODING_V1,
    ));
    let auth = model_surface_direct_auth();
    let outcome = handle_mcp_request(
        &runtime,
        rpc("tools/list", Some(Value::from(74)), json!({})),
        Some(&auth),
    )
    .await;
    let value = match outcome {
        McpOutcome::Ok(v) => v,
        other => panic!("expected Ok, got {:?}", other),
    };
    let names: Vec<&str> = value["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        crate::tool_runtime::tool_definition::LOCAL_CODING_TOOL_NAMES
    );
}

#[tokio::test]
async fn explicit_adaptive_runtime_v1_reports_adaptive_surface() {
    let runtime = test_runtime_from_model_surface_env(Some(
        crate::model_surface::MCP_MODEL_SURFACE_ADAPTIVE_RUNTIME_V1,
    ));
    let outcome = handle_mcp_request(
        &runtime,
        rpc("initialize", Some(Value::from(745)), json!({})),
        None,
    )
    .await;
    let McpOutcome::Ok(value) = outcome else {
        panic!("adaptive initialize must succeed");
    };
    assert_eq!(
        value["result"]["serverInfo"]["runtimeExposure"],
        crate::model_surface::MODEL_SURFACE_ADAPTIVE_RUNTIME
    );
}

#[tokio::test]
async fn explicit_full_operator_v1_reports_full_operator_surface() {
    let runtime = test_runtime_from_model_surface_env(Some(
        crate::model_surface::MCP_MODEL_SURFACE_FULL_OPERATOR_V1,
    ));
    let outcome = handle_mcp_request(
        &runtime,
        rpc("initialize", Some(Value::from(75)), json!({})),
        None,
    )
    .await;
    let value = match outcome {
        McpOutcome::Ok(v) => v,
        other => panic!("expected Ok, got {:?}", other),
    };
    assert_eq!(
        value["result"]["serverInfo"]["runtimeExposure"],
        crate::model_surface::MODEL_SURFACE_FULL_OPERATOR_RUNTIME
    );
}

#[tokio::test]
async fn selected_surface_is_immutable_after_environment_changes() {
    let local = test_runtime_from_model_surface_env(None);
    let local_auth = model_surface_direct_auth();
    // Prove the already-built runtime stays local_coding while the process env
    // actively requests the opposite surface; restore it before any await.
    with_model_surface_env(
        Some(crate::model_surface::MCP_MODEL_SURFACE_FULL_OPERATOR_V1),
        || assert_eq!(local.model_surface(), Some(ModelSurface::LocalCoding)),
    );
    for method in ["initialize", "tools/list"] {
        let outcome = handle_mcp_request(
            &local,
            rpc(method, Some(json!(80)), json!({})),
            Some(&local_auth),
        )
        .await;
        let McpOutcome::Ok(value) = outcome else {
            panic!("{method} must succeed");
        };
        if method == "initialize" {
            assert_eq!(
                value["result"]["serverInfo"]["runtimeExposure"],
                crate::model_surface::MODEL_SURFACE_LOCAL_CODING
            );
        } else {
            let names: Vec<&str> = value["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect();
            assert_eq!(
                names,
                crate::tool_runtime::tool_definition::LOCAL_CODING_TOOL_NAMES
            );
        }
    }
    let denied = handle_mcp_request(
        &local,
        rpc(
            "tools/call",
            Some(json!(81)),
            json!({"name": "start_coding_task", "arguments": {}}),
        ),
        None,
    )
    .await;
    assert!(matches!(denied, McpOutcome::BadRequest(_)));
    let status = local.runtime_status(None).await;
    assert_eq!(
        status.output["runtime_exposure"],
        crate::model_surface::MODEL_SURFACE_LOCAL_CODING
    );

    let full = test_runtime_with_surface(ModelSurface::FullOperatorRuntime);
    with_model_surface_env(Some("broken-after-startup"), || {
        assert_eq!(
            full.model_surface(),
            Some(ModelSurface::FullOperatorRuntime)
        );
    });
    let listed = handle_mcp_request(
        &full,
        rpc("tools/list", Some(json!(82)), mcp_2026_params(json!({}))),
        None,
    )
    .await;
    let McpOutcome::Ok(value) = listed else {
        panic!("full operator tools/list must remain available");
    };
    let expected = mcp_tools_list_payload_with_features_for_auth(
        ModelSurface::FullOperatorRuntime,
        false,
        false,
        true,
        true,
        None,
    );
    assert_eq!(
        value["result"]["tools"].as_array().unwrap().len(),
        expected["tools"].as_array().unwrap().len()
    );
    assert_eq!(
        full.runtime_status(None).await.output["runtime_exposure"],
        crate::model_surface::MODEL_SURFACE_FULL_OPERATOR_RUNTIME
    );
}

#[tokio::test]
async fn local_coding_list_manifest_and_catalog_are_identical() {
    let runtime = test_runtime_with_surface(ModelSurface::LocalCoding);
    let auth = model_surface_direct_auth();
    let listed = handle_mcp_request(
        &runtime,
        rpc("tools/list", Some(json!(83)), json!({})),
        Some(&auth),
    )
    .await;
    let McpOutcome::Ok(value) = listed else {
        panic!("tools/list must succeed");
    };
    let listed_names: Vec<&str> = value["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    let manifest = runtime
        .dispatch(crate::tool_runtime::ToolCall::ToolManifest {
            tool_name: None,
            category: None,
            intent: Some("coding".to_string()),
            include_recommended_flows: false,
            include_risk_summary: false,
        })
        .await;
    let manifest_names: Vec<&str> = manifest.output["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        listed_names,
        crate::tool_runtime::tool_definition::LOCAL_CODING_TOOL_NAMES
    );
    assert_eq!(
        manifest_names,
        crate::tool_runtime::tool_definition::LOCAL_CODING_TOOL_NAMES
    );
}
