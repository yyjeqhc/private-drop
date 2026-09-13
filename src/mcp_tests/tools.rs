use super::*;

async fn wait_for_mcp_agent_request(
    registry: &crate::runner_http::RunnerRegistry,
    client_id: &str,
    runner_instance_id: &str,
    label: &str,
) -> crate::runner_protocol::RunnerRequest {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(request) = registry
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
            "{label} did not dispatch within 10 seconds"
        );
        tokio::task::yield_now().await;
    }
}

// The compact switch is read per tools/list request, so `WEBCODEX_MCP_COMPACT_SCHEMAS`
// must stay stable (and serialized against other env-mutating tests) for the whole
// async body below. The full-operator surface is passed explicitly instead of via env.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn mcp_tools_list_returns_same_names_as_runtime() {
    // Name parity with the runtime registry must hold for stateless-2026 under
    // both full and compact schema modes. Legacy unauthenticated MCP intentionally
    // omits the stateless-only export_project_artifact transport adapter and the
    // scope-gated plugin_tool gateway. Schema shape is
    // covered by dedicated tests:
    // `mcp_tools_list_explicit_full_projection_retains_output_schema` and
    // `mcp_tools_list_compact_omits_output_schema_only`.
    let mut env = crate::test_support::TestEnvGuard::new();
    let runtime = test_runtime_with_surface(ModelSurface::FullOperatorRuntime);
    let runtime_names: Vec<String> = registered_tool_specs()
        .iter()
        .map(|s| s.name.clone())
        .collect();
    assert!(runtime_names.iter().any(|name| name == "list_runners"));
    assert!(runtime_names.iter().any(|name| name == "plugin_tool"));
    assert!(!runtime_names.iter().any(|name| name == "list_agents"));
    let legacy_runtime_names: Vec<String> = runtime_names
        .iter()
        .filter(|name| !matches!(name.as_str(), "export_project_artifact" | "plugin_tool"))
        .cloned()
        .collect();
    let stateless_runtime_names = mcp_tools_list_payload_with_features_for_auth(
        ModelSurface::FullOperatorRuntime,
        false,
        false,
        true,
        true,
        None,
    )["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();

    for compact in [false, true] {
        if compact {
            env.set("WEBCODEX_MCP_COMPACT_SCHEMAS", "true");
        } else {
            // FullOperatorRuntime preserves the historical full-schema default
            // when the override is unset.
            env.remove("WEBCODEX_MCP_COMPACT_SCHEMAS");
        }
        let outcome = handle_mcp_request(
            &runtime,
            rpc("tools/list", Some(Value::from(3)), json!({})),
            None,
        )
        .await;
        let value = match outcome {
            McpOutcome::Ok(v) => v,
            other => panic!("expected Ok (compact={compact}), got {other:?}"),
        };
        let tools = value["result"]["tools"].as_array().unwrap();
        let names: Vec<String> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            names, legacy_runtime_names,
            "legacy unauthenticated tools/list must equal runtime registry minus transport-only and scope-gated gateways (compact={compact})"
        );
        assert!(names.iter().any(|name| name == "list_runners"));
        assert!(!names.iter().any(|name| name == "plugin_tool"));
        assert!(!names.iter().any(|name| name == "list_agents"));

        let stateless_outcome = handle_mcp_request(
            &runtime,
            rpc(
                "tools/list",
                Some(Value::from(3003)),
                mcp_2026_params(json!({})),
            ),
            None,
        )
        .await;
        let stateless_value = match stateless_outcome {
            McpOutcome::Ok(value) => value,
            other => panic!("expected stateless Ok (compact={compact}), got {other:?}"),
        };
        let stateless_names: Vec<String> = stateless_value["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            stateless_names, stateless_runtime_names,
            "stateless-2026 unauthenticated tools/list must equal the full runtime registry plus fixed Skill runtime tools; Memory tools require explicit authority (compact={compact})"
        );
        assert!(stateless_names.iter().any(|name| name == "list_runners"));
        assert!(!stateless_names.iter().any(|name| name == "list_agents"));
        for tool in stateless_value["result"]["tools"].as_array().unwrap() {
            let properties = tool["inputSchema"]["properties"].as_object().unwrap();
            let recorder = properties
                .get(crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD)
                .unwrap_or_else(|| {
                    panic!("stateless recorder metadata missing for {}", tool["name"])
                });
            assert_eq!(recorder["type"], "string");
            assert_eq!(recorder["pattern"], "^wc_sess_[A-Za-z0-9_]+$");
            let description = recorder["description"].as_str().unwrap();
            assert!(description.contains("record this call"));
            assert!(description.contains("trusted collaboration provenance"));
            assert!(description.contains("Separate from any tool business Session input"));
            assert!(description.contains("grants no authority"));
            assert!(description.contains("removed before concrete parsing"));
            let ack = properties
                .get(crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD)
                .unwrap_or_else(|| panic!("stateless ACK metadata missing for {}", tool["name"]));
            assert_eq!(ack["type"], "array");
            assert_eq!(
                ack["maxItems"],
                crate::tool_runtime::sessions::MAX_TOOL_CALL_ACK_MESSAGE_IDS
            );
            assert_eq!(ack["items"]["pattern"], "^wc_msg_[A-Za-z0-9_]+$");
            let ack_description = ack["description"].as_str().unwrap();
            assert!(ack_description.contains("current model context still retains"));
            assert!(ack_description.contains("Repeat while retained"));
            assert!(ack_description.contains("If later omitted"));
            assert!(ack_description.contains("neither resolves messages nor grants authority"));
            assert!(!properties.contains_key("_session_id"));
        }
        // Exercise the real env adapter, not just the pure renderer: compact
        // must change outputSchema shape while preserving the common fields.
        // Legacy MCP keeps its historical public schema; recorder metadata is
        // a stateless-2026 transport projection only.
        for tool in tools {
            assert!(tool["name"].is_string());
            assert!(tool["description"].is_string());
            assert!(tool["inputSchema"].is_object());
            let properties = tool["inputSchema"]["properties"].as_object().unwrap();
            assert!(!properties
                .contains_key(crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD));
            assert!(!properties.contains_key("_session_id"));
            if compact {
                assert!(
                    tool.get("outputSchema").is_none(),
                    "compact env adapter must omit outputSchema for {}",
                    tool["name"]
                );
            } else {
                assert!(
                    tool["outputSchema"].is_object(),
                    "FullOperator unset env adapter must retain outputSchema for {}",
                    tool["name"]
                );
            }
        }
    }
}

#[test]
fn memory_tools_are_stateless_full_operator_only_scope_filtered_and_schema_static() {
    let generic_names = registered_tool_specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    for name in [
        "memory_search",
        "memory_read",
        "memory_set",
        "memory_delete",
        "memory_scope_list",
        "memory_scope_purge",
    ] {
        assert!(!generic_names.iter().any(|generic| generic == name));
    }

    let render = |auth: Option<&crate::auth::AuthContext>| {
        let mut payload = mcp_tools_list_payload_with_features_for_auth(
            ModelSurface::FullOperatorRuntime,
            false,
            false,
            true,
            true,
            auth,
        );
        add_stateless_workflow_recorder_metadata(&mut payload, ModelSurface::FullOperatorRuntime);
        payload
    };
    let memory_names = |payload: &Value| {
        payload["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .filter(|name| name.starts_with("memory_"))
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let oauth = |scopes: &[&str]| crate::auth::AuthContext {
        scopes: scopes.iter().map(|scope| (*scope).to_string()).collect(),
        ..crate::auth::AuthContext::new(crate::auth::AuthKind::OAuth2Token)
    };

    assert!(memory_names(&render(None)).is_empty());
    assert!(memory_names(&render(Some(&oauth(&[crate::auth::SCOPE_PROJECT_READ])))).is_empty());
    assert!(memory_names(&render(Some(&oauth(&[crate::auth::SCOPE_MEMORY_READ])))).is_empty());
    assert_eq!(
        memory_names(&render(Some(&oauth(&[
            crate::auth::SCOPE_PROJECT_READ,
            crate::auth::SCOPE_MEMORY_READ,
        ])))),
        vec!["memory_search", "memory_read"]
    );
    assert!(memory_names(&render(Some(&oauth(&[crate::auth::SCOPE_PROJECT_WRITE])))).is_empty());
    assert!(memory_names(&render(Some(&oauth(&[crate::auth::SCOPE_MEMORY_MANAGE])))).is_empty());
    assert_eq!(
        memory_names(&render(Some(&oauth(&[
            crate::auth::SCOPE_PROJECT_WRITE,
            crate::auth::SCOPE_MEMORY_MANAGE,
        ])))),
        vec!["memory_set", "memory_delete"]
    );
    let full_auth = oauth(&[
        crate::auth::SCOPE_PROJECT_READ,
        crate::auth::SCOPE_MEMORY_READ,
        crate::auth::SCOPE_PROJECT_WRITE,
        crate::auth::SCOPE_MEMORY_MANAGE,
    ]);
    let full = render(Some(&full_auth));
    // Project Memory manage authority is not global lifecycle authority.
    assert!(!memory_names(&full).contains(&"memory_scope_list".to_string()));
    assert!(!memory_names(&full).contains(&"memory_scope_purge".to_string()));
    let admin_auth = oauth(&[crate::auth::SCOPE_ADMIN]);
    let admin = render(Some(&admin_auth));
    assert!(memory_names(&admin).contains(&"memory_scope_list".to_string()));
    assert!(memory_names(&admin).contains(&"memory_scope_purge".to_string()));
    assert_eq!(
        memory_names(&full),
        vec![
            "memory_search",
            "memory_read",
            "memory_set",
            "memory_delete"
        ]
    );

    let open = crate::auth::open_anonymous_context();
    assert!(memory_names(&render(Some(&open))).is_empty());
    let project_credential =
        crate::auth::shared_key::project_credential_context("wc_pgrant_memorytools");
    assert!(memory_names(&render(Some(&project_credential))).is_empty());
    let direct = crate::auth::shared_key_context("memory-tools-direct-shared-key");
    assert_eq!(
        memory_names(&render(Some(&direct))),
        vec![
            "memory_search",
            "memory_read",
            "memory_set",
            "memory_delete"
        ]
    );
    let project_share = crate::auth::AuthContext {
        kind: crate::auth::AuthKind::OAuth2Token,
        scopes: crate::auth::project_share::PROJECT_SHARE_OAUTH_SCOPES
            .iter()
            .map(|scope| (*scope).to_string())
            .collect(),
        token_kind: Some(crate::auth::project_share::PROJECT_SHARE_OAUTH_TOKEN_KIND.to_string()),
        ..crate::auth::AuthContext::new(crate::auth::AuthKind::OAuth2Token)
    };
    assert!(memory_names(&render(Some(&project_share))).is_empty());

    let memory_search = full["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "memory_search")
        .unwrap();
    let context_request = &memory_search["inputSchema"]["properties"]["context_request"];
    assert_eq!(context_request["items"]["type"], "string");
    assert!(context_request["items"].get("enum").is_none());
    let description = context_request["description"].as_str().unwrap();
    assert!(description.contains("after this tool's main effect/observation"));
    assert!(description.contains("grants no authority"));
    assert!(description.contains("retroactive precondition"));
    for key in [
        "project.instructions",
        "webcodex.workflow",
        "skills.catalog",
        "plugins.catalog",
        "memory.bootstrap",
    ] {
        assert!(description.contains(key));
    }
    assert!(
        memory_search["outputSchema"]["properties"]["output"]["properties"]["memories"].is_object()
    );

    let set_description = full["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "memory_set")
        .and_then(|tool| tool["description"].as_str())
        .unwrap();
    for required in [
        "project:write",
        "memory:manage",
        "permission",
        "credentials",
        "execution authority",
    ] {
        assert!(
            set_description.contains(required),
            "memory_set: {set_description}"
        );
    }

    assert_eq!(
        full,
        render(Some(&full_auth)),
        "Memory schemas are record-content independent"
    );
    assert_eq!(
        crate::tool_runtime::memory_runtime_tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>(),
        vec!["memory_search", "memory_read"]
    );
    assert_eq!(
        crate::tool_runtime::memory_management_tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>(),
        vec![
            "memory_set",
            "memory_delete",
            "memory_scope_list",
            "memory_scope_purge"
        ]
    );

    let local = mcp_tools_list_payload_with_features_for_auth(
        ModelSurface::LocalCoding,
        false,
        false,
        true,
        true,
        Some(&direct),
    );
    assert!(memory_names(&local).is_empty());
    let project_connector = project_connector_tools_list_payload_with_compact(false);
    assert!(memory_names(&project_connector).is_empty());
    let legacy_full = mcp_tools_list_payload_with_compact(ModelSurface::FullOperatorRuntime, false);
    assert!(memory_names(&legacy_full).is_empty());
}

#[test]
fn trace_reader_is_stateless_operator_only_admin_scoped_and_schema_static() {
    let render =
        |surface: ModelSurface, stateless_2026: bool, auth: Option<&crate::auth::AuthContext>| {
            mcp_tools_list_payload_with_features_for_auth(
                surface,
                false,
                false,
                true,
                stateless_2026,
                auth,
            )
        };
    let names = |payload: &Value| {
        payload["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect::<Vec<_>>()
    };
    let admin = crate::auth::AuthContext {
        role: Some("admin".to_string()),
        scopes: vec![crate::auth::SCOPE_ADMIN.to_string()],
        is_bootstrap: true,
        ..crate::auth::AuthContext::new(crate::auth::AuthKind::Bootstrap)
    };
    let ordinary = crate::auth::AuthContext {
        scopes: vec![crate::auth::SCOPE_PROJECT_READ.to_string()],
        ..crate::auth::AuthContext::new(crate::auth::AuthKind::OAuth2Token)
    };

    assert!(
        !names(&render(ModelSurface::FullOperatorRuntime, true, None))
            .contains(&"read_tool_trace".to_string())
    );
    assert!(!names(&render(
        ModelSurface::FullOperatorRuntime,
        true,
        Some(&ordinary)
    ))
    .contains(&"read_tool_trace".to_string()));
    assert!(!names(&render(
        ModelSurface::FullOperatorRuntime,
        false,
        Some(&admin)
    ))
    .contains(&"read_tool_trace".to_string()));
    assert!(
        !names(&render(ModelSurface::LocalCoding, true, Some(&admin)))
            .contains(&"read_tool_trace".to_string())
    );

    let full = render(ModelSurface::FullOperatorRuntime, true, Some(&admin));
    let tool = full["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "read_tool_trace")
        .expect("admin Stateless Full Operator trace reader");
    assert_eq!(tool["inputSchema"]["required"], json!(["trace_ref"]));
    assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    assert!(tool["inputSchema"]["properties"]["payload_index"].is_object());
    assert!(tool["outputSchema"]["properties"]["output"]["properties"]["payload"].is_object());

    let adaptive = render(ModelSurface::AdaptiveRuntime, true, Some(&admin));
    let adaptive_names = names(&adaptive);
    assert!(!adaptive_names.contains(&"read_tool_trace".to_string()));
}

#[test]
fn skill_runtime_tools_are_stateless_full_operator_only_and_schema_static() {
    let generic_names = registered_tool_specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    assert!(!generic_names.iter().any(|name| name == "skill_list"));
    assert!(!generic_names.iter().any(|name| name == "skill_read_file"));

    let render_full = || {
        let mut payload = mcp_tools_list_payload_with_features_for_auth(
            ModelSurface::FullOperatorRuntime,
            false,
            false,
            true,
            true,
            None,
        );
        add_stateless_workflow_recorder_metadata(&mut payload, ModelSurface::FullOperatorRuntime);
        payload
    };
    let before = render_full();
    let skill_names = before["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .filter(|name| name.starts_with("skill_"))
        .collect::<Vec<_>>();
    assert_eq!(skill_names, vec!["skill_list", "skill_read_file"]);

    let skill_list = before["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "skill_list")
        .unwrap();
    assert_eq!(
        skill_list["inputSchema"]["properties"]["limit"]["maximum"],
        64
    );
    let context_request = &skill_list["inputSchema"]["properties"]["context_request"];
    assert_eq!(context_request["items"]["type"], "string");
    assert!(context_request["items"].get("enum").is_none());
    assert_eq!(
        skill_list["outputSchema"]["properties"]["output"]["properties"]["skills"]["type"],
        "array"
    );

    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".agents/skills/foo")).unwrap();
    std::fs::write(
        tmp.path().join(".agents/skills/foo/SKILL.md"),
        "---\nname: foo\ndescription: first\n---\nbody\n",
    )
    .unwrap();
    let one_package = render_full();
    std::fs::create_dir_all(tmp.path().join(".agents/skills/bar")).unwrap();
    std::fs::write(
        tmp.path().join(".agents/skills/bar/SKILL.md"),
        "---\nname: bar\ndescription: second\n---\nbody\n",
    )
    .unwrap();
    let two_packages = render_full();
    assert_eq!(before, one_package);
    assert_eq!(
        one_package, two_packages,
        "Skill package count must not alter MCP tool schemas"
    );

    for payload in [
        mcp_tools_list_payload_with_features_for_auth(
            ModelSurface::LocalCoding,
            false,
            false,
            true,
            true,
            None,
        ),
        project_connector_tools_list_payload_with_compact(false),
    ] {
        assert!(payload["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| !matches!(
                tool["name"].as_str(),
                Some("skill_list" | "skill_read_file")
            )));
    }
    let legacy_full = mcp_tools_list_payload_with_compact(ModelSurface::FullOperatorRuntime, false);
    assert!(legacy_full["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|tool| !matches!(
            tool["name"].as_str(),
            Some("skill_list" | "skill_read_file")
        )));
}

#[test]
fn skill_management_tools_require_admin_and_remain_fixed_schema() {
    let render = |auth: Option<&crate::auth::AuthContext>| {
        mcp_tools_list_payload_with_features_for_auth(
            ModelSurface::FullOperatorRuntime,
            false,
            false,
            true,
            true,
            auth,
        )
    };
    let shared = crate::auth::shared_key_context("skill-management-test");
    let shared_payload = render(Some(&shared));
    let shared_names = shared_payload["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .filter(|name| name.starts_with("skill_"))
        .collect::<Vec<_>>();
    assert_eq!(shared_names, vec!["skill_list", "skill_read_file"]);

    let admin = crate::auth::AuthContext {
        role: Some("admin".to_string()),
        scopes: vec![crate::auth::SCOPE_ADMIN.to_string()],
        is_bootstrap: true,
        ..crate::auth::AuthContext::new(crate::auth::AuthKind::Bootstrap)
    };
    let first = render(Some(&admin));
    let names = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .filter(|name| name.starts_with("skill_"))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "skill_list",
            "skill_read_file",
            "skill_versions",
            "skill_install",
            "skill_activate",
            "skill_remove_revision",
        ]
    );
    assert_eq!(
        crate::tool_runtime::skill_management_tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>(),
        vec![
            "skill_versions",
            "skill_install",
            "skill_activate",
            "skill_remove_revision",
        ]
    );
    for name in ["skill_install", "skill_activate", "skill_remove_revision"] {
        let description = first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .and_then(|tool| tool["description"].as_str())
            .unwrap_or_else(|| panic!("missing {name} retention description"));
        for required in ["24 hours", "7 days", "skill_versions", "not proof"] {
            assert!(
                description.contains(required),
                "{name} must document replay retention: {description}"
            );
        }
    }
    assert_eq!(
        first,
        render(Some(&admin)),
        "management schemas are content-independent"
    );
}

#[test]
fn stateless_workflow_recorder_metadata_does_not_expand_project_connector_or_local_schema() {
    let project_connector = project_connector_tools_list_payload_with_compact(false);
    for tool in project_connector["tools"].as_array().unwrap() {
        let properties = tool["inputSchema"]["properties"].as_object().unwrap();
        assert!(!properties
            .contains_key(crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD));
        assert!(!properties
            .contains_key(crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD));
        assert!(!properties.contains_key(
            crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD
        ));
        assert!(!properties.contains_key(
            crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD
        ));
    }

    let mut local = mcp_tools_list_payload_with_compact(ModelSurface::LocalCoding, false);
    add_stateless_workflow_recorder_metadata(&mut local, ModelSurface::LocalCoding);
    assert!(local["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|tool| tool["inputSchema"]["properties"]
            .get(crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD)
            .is_none()));

    assert!(local["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|tool| tool["inputSchema"]["properties"]
            .get(crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD)
            .is_none()));

    let mut full = mcp_tools_list_payload_with_compact(ModelSurface::FullOperatorRuntime, false);
    add_stateless_workflow_recorder_metadata(&mut full, ModelSurface::FullOperatorRuntime);
    for name in [
        "read_files",
        "search_project_texts",
        "tool_manifest",
        "show_changes",
        "work_on_project",
        "workspace_hygiene_check",
    ] {
        let tool = full["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing {name} schema"));
        let properties = tool["inputSchema"]["properties"].as_object().unwrap();
        assert!(
            !properties.contains_key(
                crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD
            ),
            "{name} must not advertise context ACK"
        );
        assert!(properties.contains_key(
            crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD
        ));
        assert!(properties
            .contains_key(crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD));
        assert!(properties
            .contains_key(crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD));
        assert!(properties.contains_key(
            crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD
        ));
    }
    for name in ["session_handoff_summary", "apply_text_edits", "run_process"] {
        let tool = full["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing {name} schema"));
        let properties = tool["inputSchema"]["properties"].as_object().unwrap();
        assert!(properties.contains_key(
            crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD
        ));
        assert_eq!(
            properties["ack_session_context_revision"]["description"],
            "Echo the latest retained session_context_revision; omit when unknown."
        );
        assert!(
            tool["outputSchema"]["properties"]["output"]["properties"]["session_continuity"]
                ["properties"]["status"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("recovered"))
        );
        assert!(properties.contains_key(
            crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD
        ));
    }
    for tool in full["tools"].as_array().unwrap() {
        if tool["name"] != "work_on_project" {
            let serialized = serde_json::to_string(tool).unwrap();
            assert!(
                !serialized.contains("work_on_project"),
                "{} pollutes exact work_on_project discovery",
                tool["name"]
            );
        }
    }
    let read_files = full["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "read_files")
        .expect("full-operator read_files schema");
    let read_files_output = serde_json::to_string(&read_files["outputSchema"]).unwrap();
    assert!(read_files_output.contains("context_projection"));
    assert!(read_files_output.contains("post_tool"));
    let list_tools = full["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "list_tools")
        .expect("full-operator list_tools schema");
    let context_output =
        &list_tools["outputSchema"]["properties"]["output"]["properties"]["context_projection"];
    assert_eq!(context_output["type"], "object");
    assert_eq!(context_output["properties"]["timing"]["const"], "post_tool");
    assert_eq!(
        context_output["properties"]["applies_to_current_effect"]["const"],
        false
    );

    let generic = registered_tool_specs()
        .into_iter()
        .find(|tool| tool.name == "complete_session_message")
        .expect("generic complete_session_message spec");
    let generic_properties = generic.input_schema["properties"].as_object().unwrap();
    assert!(!generic_properties
        .contains_key(crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD));
    assert!(!generic_properties
        .contains_key(crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD));
    assert!(!generic_properties
        .contains_key(crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD));
    assert!(!generic_properties
        .contains_key(crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD));
    assert!(!generic_properties
        .contains_key(crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD));
}

#[test]
fn stateless_ack_wrapper_normalizes_and_is_removed_before_concrete_tool_parsing() {
    let mut arguments = json!({
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD: [
            "wc_msg_beta",
            "wc_msg_beta",
            "wc_msg_alpha"
        ]
    });
    let normalized = strip_stateless_ack_session_message_ids(&mut arguments).unwrap();
    assert_eq!(normalized, vec!["wc_msg_beta", "wc_msg_alpha"]);
    assert!(arguments
        .get(crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD)
        .is_none());
    assert_eq!(normalized, vec!["wc_msg_beta", "wc_msg_alpha"]);
    crate::tool_runtime::ToolCall::from_tool_name("list_tools", arguments)
        .expect("wrapper ACK metadata must be gone before concrete parsing");

    let mut malformed = json!({
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD: ["not-a-message-id"]
    });
    assert!(strip_stateless_ack_session_message_ids(&mut malformed).is_err());
    let mut oversized = json!({
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD:
            (0..=crate::tool_runtime::sessions::MAX_TOOL_CALL_ACK_MESSAGE_IDS)
                .map(|index| format!("wc_msg_{index}"))
                .collect::<Vec<_>>()
    });
    assert!(strip_stateless_ack_session_message_ids(&mut oversized).is_err());
}

#[test]
fn stateless_message_resolution_wrapper_is_validated_and_removed_before_concrete_parsing() {
    let mut arguments = json!({
        crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD: {
            "message_id": "wc_msg_beta",
            "resolution": "  handled in the current model turn  "
        }
    });
    let resolution = strip_stateless_session_message_resolution(&mut arguments)
        .unwrap()
        .expect("message resolution wrapper");
    assert_eq!(resolution.message_id, "wc_msg_beta");
    assert_eq!(resolution.resolution, "handled in the current model turn");
    assert!(arguments
        .get(crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD)
        .is_none());
    crate::tool_runtime::ToolCall::from_tool_name("list_tools", arguments)
        .expect("message resolution wrapper metadata must be gone before concrete parsing");

    for malformed in [
        json!({
            crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD: {
                "message_id": "not-a-message-id",
                "resolution": "handled"
            }
        }),
        json!({
            crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD: {
                "message_id": "wc_msg_beta",
                "resolution": "   "
            }
        }),
        json!({
            crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD: {
                "message_id": "wc_msg_beta",
                "resolution": "handled",
                "extra": true
            }
        }),
    ] {
        let mut malformed = malformed;
        assert!(strip_stateless_session_message_resolution(&mut malformed).is_err());
    }
}

#[test]
fn stateless_context_request_is_deduped_open_ended_and_removed_before_parsing() {
    let mut arguments = json!({
        crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD: [
            "project.instructions",
            "future.material",
            "project.instructions"
        ]
    });
    let normalized = strip_stateless_context_request(&mut arguments).unwrap();
    assert_eq!(
        normalized,
        vec![
            "project.instructions".to_string(),
            "future.material".to_string()
        ]
    );
    assert!(arguments
        .get(crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD)
        .is_none());
    crate::tool_runtime::ToolCall::from_tool_name("list_tools", arguments)
        .expect("context_request wrapper metadata must be gone before concrete parsing");

    for malformed in [
        json!({crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD: "project.instructions"}),
        json!({crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD: ["bad key"]}),
        json!({crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD: [""]}),
        json!({crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD:
            (0..=crate::tool_runtime::context_projection::MAX_CONTEXT_REQUEST_ITEMS)
                .map(|index| format!("future.material.{index}"))
                .collect::<Vec<_>>()
        }),
    ] {
        let mut malformed = malformed;
        assert!(strip_stateless_context_request(&mut malformed).is_err());
    }
}

#[test]
fn stateless_context_revision_ack_is_request_scoped_and_removed_before_parsing() {
    let mut arguments = json!({
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD: 41,
    });
    let ack = strip_stateless_ack_session_context_revision(&mut arguments).unwrap();
    assert_eq!(ack, json!(41));
    assert!(arguments
        .get(crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD)
        .is_none());
    assert_eq!(
        session_context_revision_ack_from_wire(Some(ack)),
        crate::tool_runtime::sessions::SessionContextRevisionAck::Revision(41)
    );
    crate::tool_runtime::ToolCall::from_tool_name("list_tools", arguments)
        .expect("context revision wrapper metadata must be gone before concrete parsing");

    assert_eq!(
        session_context_revision_ack_from_wire(None),
        crate::tool_runtime::sessions::SessionContextRevisionAck::Unacknowledged
    );

    let mut malformed = json!({
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD: "not-a-revision",
    });
    let malformed_ack = strip_stateless_ack_session_context_revision(&mut malformed).unwrap();
    assert_eq!(
        session_context_revision_ack_from_wire(Some(malformed_ack)),
        crate::tool_runtime::sessions::SessionContextRevisionAck::Invalid
    );
}

#[test]
fn reobservable_tool_strips_cached_context_ack_without_accepting_it() {
    let mut arguments = json!({
        "project": "proj",
        "items": [{"path": "src/lib.rs"}],
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD: 41,
    });
    let ack = strip_stateless_ack_session_context_revision(&mut arguments).unwrap();
    assert_eq!(ack, json!(41));
    assert!(arguments
        .get(crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD)
        .is_none());

    let accepts_ack =
        crate::tool_runtime::tool_definition::runtime_tool_accepts_context_ack("read_files");
    assert!(!accepts_ack);
    assert_eq!(
        session_context_revision_ack_from_wire(Some(ack)),
        crate::tool_runtime::sessions::SessionContextRevisionAck::Revision(41)
    );

    crate::tool_runtime::ToolCall::from_tool_name("read_files", arguments)
        .expect("cached ACK must be stripped before concrete read_files parsing");
}

#[test]
fn stateless_invocation_metadata_stays_typed_and_business_arguments_stay_clean() {
    let mut arguments = json!({
        "project": "proj",
        "items": [{"path": "src/lib.rs"}],
        crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD: "wc_sess_adapter",
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD: ["wc_msg_alpha"],
        crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD: {
            "message_id": "wc_msg_alpha",
            "resolution": "handled"
        },
        crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD: ["webcodex.workflow"],
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD: 7,
    });
    let recording_session_id = strip_recording_session_id(&mut arguments).unwrap();
    let ack_session_message_ids = strip_stateless_ack_session_message_ids(&mut arguments).unwrap();
    let session_message_resolution =
        strip_stateless_session_message_resolution(&mut arguments).unwrap();
    let context_request = strip_stateless_context_request(&mut arguments).unwrap();
    let ack_session_context_revision = session_context_revision_ack_from_wire(
        strip_stateless_ack_session_context_revision(&mut arguments),
    );
    let metadata = crate::tool_runtime::kernel::ToolInvocationMetadata {
        ack_session_message_ids,
        session_message_resolution,
        context_request,
        ack_session_context_revision,
    };

    assert_eq!(recording_session_id.as_deref(), Some("wc_sess_adapter"));
    assert_eq!(metadata.ack_session_message_ids, vec!["wc_msg_alpha"]);
    assert_eq!(metadata.context_request, vec!["webcodex.workflow"]);
    assert_eq!(
        metadata.ack_session_context_revision,
        crate::tool_runtime::sessions::SessionContextRevisionAck::Revision(7)
    );
    assert!(metadata.session_message_resolution.is_some());
    for field in [
        crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD,
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD,
        crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD,
        crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD,
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD,
    ] {
        assert!(
            arguments.get(field).is_none(),
            "wrapper leaked into business args: {field}"
        );
    }
    assert!(!arguments.to_string().contains("__webcodex_"));
    crate::tool_runtime::ToolCall::from_tool_name("read_files", arguments)
        .expect("typed invocation metadata must not be required for concrete ToolCall parsing");
}

#[test]
fn mcp_tools_list_adds_image_mode_without_changing_generic_artifact_schema() {
    // Explicit non-compact rendering: no env involvement, nothing to serialize.
    let payload = mcp_tools_list_payload_with_compact(ModelSurface::FullOperatorRuntime, false);
    let mcp_tool = payload["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "read_project_artifact")
        .expect("MCP read_project_artifact");
    assert_eq!(
        mcp_tool["inputSchema"]["properties"]["as_image"]["type"],
        "boolean"
    );
    assert!(mcp_tool["description"]
        .as_str()
        .unwrap()
        .contains("as_image=true"));

    let generic_tool = registered_tool_specs()
        .into_iter()
        .find(|tool| tool.name == "read_project_artifact")
        .expect("generic read_project_artifact");
    assert!(
        generic_tool.input_schema["properties"]
            .get("as_image")
            .is_none(),
        "MCP image presentation must not change the generic REST/GPT Actions schema"
    );
}

#[test]
fn mcp_tools_list_exposes_host_file_params_for_conversation_import() {
    let payload = mcp_tools_list_payload_with_compact(ModelSurface::FullOperatorRuntime, false);
    let tool = payload["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "import_conversation_files_to_project")
        .expect("MCP conversation import tool");

    assert_eq!(
        tool["_meta"]["openai/fileParams"],
        json!(["openaiFileIdRefs"])
    );
    let refs = &tool["inputSchema"]["properties"]["openaiFileIdRefs"];
    assert_eq!(refs["type"], "array");
    assert_eq!(refs["minItems"], 1);
    assert_eq!(refs["maxItems"], 10);
    assert_eq!(
        refs["items"]["required"],
        json!(["download_url", "file_id"])
    );
    for property in ["download_url", "file_id", "mime_type", "file_name"] {
        assert_eq!(refs["items"]["properties"][property]["type"], "string");
    }
    assert!(tool["description"]
        .as_str()
        .unwrap()
        .contains("host file-reference mechanism"));
}

#[test]
fn mcp_file_params_keep_raw_object_shape_and_reject_model_mask_strings() {
    // ChatGPT masks openai/fileParams to string[] for the model, then rewrites
    // those selections back to the raw provided-file object[] below before the
    // MCP request reaches WebCodex. WebCodex intentionally accepts only that
    // post-host-rewrite object form; it never interprets model-facing strings.
    let _string_error = crate::tool_runtime::ToolCall::from_tool_name(
        "import_conversation_files_to_project",
        json!({
            "project": "agent:test:demo",
            "openaiFileIdRefs": ["file-model-selection"]
        }),
    )
    .expect_err("model-facing string[] must not deserialize at the server");

    let call = crate::tool_runtime::ToolCall::from_tool_name(
        "import_conversation_files_to_project",
        json!({
            "project": "agent:test:demo",
            "openaiFileIdRefs": [{
                "download_url": "https://download.example/file",
                "file_id": "file_host_rewritten",
                "mime_type": "application/pdf",
                "file_name": "paper.pdf"
            }]
        }),
    )
    .expect("post-host-rewrite provided-file object[] must deserialize");
    let crate::tool_runtime::ToolCall::ImportConversationFilesToProject {
        openai_file_id_refs,
        trusted_mcp_host_file_import,
        ..
    } = call
    else {
        unreachable!()
    };
    assert_eq!(openai_file_id_refs.len(), 1);
    assert_eq!(openai_file_id_refs[0].file_id, "file_host_rewritten");
    assert!(
        !trusted_mcp_host_file_import,
        "raw input cannot set provenance"
    );
}

#[test]
fn mcp_file_import_trust_requires_exact_configured_active_client_id() {
    const CALLBACK: &str = "https://chatgpt.example/connector/oauth/test";
    let mut config = (*test_config_oauth2(Some("secret"))).clone();
    let (_tmp, db) = test_db();
    let user = seed_user(&db, "alice");

    let make_client = |name: &str, redirect_uris: &str| crate::models::OAuthClientRecord {
        id: uuid::Uuid::new_v4().to_string(),
        client_id: crate::auth::generate_oauth_client_id(),
        client_secret_hash: crate::auth::hash_token("test-secret"),
        name: name.to_string(),
        owner_user_id: Some(user.id.clone()),
        owner_project_grant_id: None,
        owner_shared_key_hash: None,
        redirect_uris: redirect_uris.to_string(),
        allowed_scopes: "project:write".to_string(),
        created_at: chrono::Utc::now().timestamp(),
        revoked_at: None,
    };
    let auth_for = |client_id: &str| {
        let mut auth = crate::auth::AuthContext::new(crate::auth::AuthKind::OAuth2Token);
        auth.username = Some("alice".to_string());
        auth.token_kind = Some("oauth2".to_string());
        auth.scopes = vec![crate::auth::SCOPE_PROJECT_WRITE.to_string()];
        auth.allowed_client_id = Some(client_id.to_string());
        auth
    };

    let trusted = make_client("ChatGPT WebCodex", CALLBACK);
    db.insert_oauth_client(&trusted).unwrap();
    config.oauth2.trusted_mcp_file_client_ids = vec![trusted.client_id.clone()];
    let trusted_auth = auth_for(&trusted.client_id);
    assert_eq!(
        mcp_host_file_import_trust_from_state(&config, &db, Some(&trusted_auth)),
        HostFileImportTrust::TrustedOAuthClient,
        "the exact configured active OAuth client ID is trusted"
    );

    let same_redirect = make_client("Different Client", CALLBACK);
    db.insert_oauth_client(&same_redirect).unwrap();
    assert_eq!(
        mcp_host_file_import_trust_from_state(
            &config,
            &db,
            Some(&auth_for(&same_redirect.client_id))
        ),
        HostFileImportTrust::Untrusted,
        "sharing a redirect URI must not grant authority"
    );
    assert_eq!(
        mcp_host_file_import_trust_from_state(&config, &db, Some(&trusted_auth)),
        HostFileImportTrust::TrustedOAuthClient,
        "multiple active clients sharing the callback must not revoke explicit client-ID trust"
    );

    let same_name = make_client("ChatGPT WebCodex", "https://other.example/callback");
    db.insert_oauth_client(&same_name).unwrap();
    assert_eq!(
        mcp_host_file_import_trust_from_state(&config, &db, Some(&auth_for(&same_name.client_id))),
        HostFileImportTrust::Untrusted,
        "sharing the display name must not grant authority"
    );

    let unknown_client_id = crate::auth::generate_oauth_client_id();
    let mut unknown_config = config.clone();
    unknown_config.oauth2.trusted_mcp_file_client_ids = vec![unknown_client_id.clone()];
    assert_eq!(
        mcp_host_file_import_trust_from_state(
            &unknown_config,
            &db,
            Some(&auth_for(&unknown_client_id))
        ),
        HostFileImportTrust::Untrusted,
        "a configured ID without an active OAuth client record must fail closed"
    );

    let mut empty_config = config.clone();
    empty_config.oauth2.trusted_mcp_file_client_ids.clear();
    assert_eq!(
        mcp_host_file_import_trust_from_state(&empty_config, &db, Some(&trusted_auth)),
        HostFileImportTrust::Untrusted,
        "empty operator trust config must fail closed"
    );

    db.revoke_oauth_client(&trusted.id, chrono::Utc::now().timestamp())
        .unwrap();
    assert_eq!(
        mcp_host_file_import_trust_from_state(&config, &db, Some(&trusted_auth)),
        HostFileImportTrust::Untrusted,
        "revoked configured client registrations must fail closed"
    );

    let replacement = make_client("ChatGPT WebCodex", CALLBACK);
    db.insert_oauth_client(&replacement).unwrap();
    assert_ne!(replacement.client_id, trusted.client_id);
    assert_eq!(
        mcp_host_file_import_trust_from_state(
            &config,
            &db,
            Some(&auth_for(&replacement.client_id))
        ),
        HostFileImportTrust::Untrusted,
        "recreating a client with the same callback cannot inherit the configured client-ID trust"
    );

    let api_auth = crate::auth::AuthContext::new(crate::auth::AuthKind::ApiToken);
    assert_eq!(
        mcp_host_file_import_trust_from_state(&config, &db, Some(&api_auth)),
        HostFileImportTrust::Untrusted
    );
}

#[test]
fn ordinary_artifact_result_keeps_existing_text_and_structured_base64_shape() {
    let value = mcp_runtime_tool_result(
        "read_project_artifact",
        false,
        ToolResult::ok(json!({
            "path": "sample.pdf",
            "mime_type": "application/pdf",
            "file_bytes": 100_000,
            "sha256": "a".repeat(64),
            "offset": 0,
            "bytes_returned": 32_768,
            "content_base64": "JVBERg==",
            "next_offset": 32_768,
            "truncated": true,
            "eof": false,
        })),
    );
    assert_eq!(value["content"].as_array().unwrap().len(), 1);
    assert_eq!(value["content"][0]["type"], "text");
    assert_eq!(
        value["structuredContent"]["output"]["content_base64"],
        "JVBERg=="
    );
    assert_eq!(
        value["structuredContent"]["output"]["truncated"], true,
        "ordinary artifact reads must retain chunk continuation metadata"
    );
}

#[tokio::test]
async fn mcp_image_call_returns_native_image_for_remote_agent_project() {
    // read_project_artifact is an artifact tool outside the local_coding
    // surface; select the full operator surface for this call.
    let runtime = test_runtime_with_surface(ModelSurface::FullOperatorRuntime);
    let client_id = "mcp-vision-agent";
    let runner_instance_id = "inst-mcp-vision";
    let project_name = "remote-images";
    runtime
        .runner_registry
        .register(crate::test_support::current_runner_registration(
            RunnerRegisterRequest {
                client_id: client_id.to_string(),
                runner_instance_id: runner_instance_id.to_string(),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: None,
                owner: None,
                hostname: None,
                host_context: None,
                capabilities: RunnerCapabilities {
                    file_read: true,
                    ..Default::default()
                },
                policy: None,
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
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        runner_instance_id,
        vec![RunnerProjectSummary {
            id: project_name.to_string(),
            name: Some(project_name.to_string()),
            path: "/remote/session-atlas".to_string(),
            allow_patch: true,
            kind: Some("repo".to_string()),
            registration_source: None,
            description: None,
            hooks: Vec::new(),
            disabled: false,
            revision: None,
            git_branch: None,
            git_head: None,
            git_dirty: None,
            updated_at: 1,
            shell_profile: None,
        }],
    )
    .await;
    let project = crate::tool_runtime::runner_project_runtime_id(client_id, project_name);
    let mut auth = crate::auth::AuthContext::new(crate::auth::AuthKind::Bootstrap);
    auth.is_bootstrap = true;

    // Larger than the ordinary 256 KiB runner stdout cap: this proves the
    // narrowly widened MCP image result path carries a real screenshot-sized
    // response without silently tail-truncating its JSON/base64.
    let mut image_bytes = vec![0u8; 300 * 1024];
    image_bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    let image_base64 = general_purpose::STANDARD.encode(&image_bytes);
    let sha256 = format!("{:x}", Sha256::digest(&image_bytes));
    let path = "docs/images/console-overview-dark.png";

    let call = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let auth = auth.clone();
        async move {
            handle_mcp_request(
                &runtime,
                rpc(
                    "tools/call",
                    Some(json!(77)),
                    json!({
                        "name": "read_project_artifact",
                        "arguments": {
                            "project": project,
                            "path": path,
                            "as_image": true
                        }
                    }),
                ),
                Some(&auth),
            )
            .await
        }
    });

    let request = wait_for_mcp_agent_request(
        &runtime.runner_registry,
        client_id,
        runner_instance_id,
        "MCP image call",
    )
    .await;
    assert_eq!(request.kind, "file_read_project_artifact");
    assert_eq!(request.cwd.as_deref(), Some("/remote/session-atlas"));
    let payload: Value = serde_json::from_str(request.content.as_deref().unwrap()).unwrap();
    assert_eq!(payload["path"], path);
    assert_eq!(payload["offset"], 0);
    assert_eq!(
        payload["length"],
        crate::artifact_policy::MAX_MCP_IMAGE_BYTES
    );
    assert_eq!(
        payload["max_file_bytes"],
        crate::artifact_policy::MAX_MCP_IMAGE_BYTES
    );
    assert_eq!(payload["mcp_image"], true);

    let stdout = json!({
        "path": path,
        "mime_type": "image/png",
        "file_bytes": image_bytes.len(),
        "sha256": sha256,
        "offset": 0,
        "bytes_returned": image_bytes.len(),
        "content_base64": &image_base64,
        "next_offset": image_bytes.len(),
        "truncated": false,
        "eof": true,
    })
    .to_string();
    assert!(stdout.len() > 256 * 1024);
    runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: client_id.to_string(),
            runner_instance_id: runner_instance_id.to_string(),
            request_id: request.request_id,
            exit_code: Some(0),
            stdout: Some(stdout),
            stderr: Some(String::new()),
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap();

    let outcome = call.await.unwrap();
    let McpOutcome::Ok(value) = outcome else {
        panic!("expected MCP tool result, got {outcome:?}");
    };
    assert_eq!(value["result"]["isError"], false);
    let content = value["result"]["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "text");
    assert!(content[0]["text"].as_str().unwrap().len() < 256);
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["mimeType"], "image/png");
    assert_eq!(content[1]["data"], image_base64);
    let structured = &value["result"]["structuredContent"];
    assert_eq!(structured["success"], true);
    assert_eq!(structured["output"]["content_delivery"], "mcp_image");
    assert!(
        structured["output"].get("content_base64").is_none(),
        "structuredContent must not duplicate the image base64"
    );
}

#[test]
fn computer_snapshot_frames_native_image_without_structured_base64() {
    let image_bytes = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46];
    let image_base64 = general_purpose::STANDARD.encode(&image_bytes);
    let result = ToolResult::ok(json!({
        "client_id": "msi",
        "surface": {
            "surface_id": "surface_test",
            "application": "Test App",
            "title": "Test Window",
            "width": 640,
            "height": 480,
            "focused": true,
            "active": true
        },
        "width": 640,
        "height": 480,
        "mime_type": "image/jpeg",
        "file_bytes": image_bytes.len(),
        "content_base64": image_base64
    }));

    let value = crate::mcp::mcp_runtime_tool_result("computer_snapshot", false, result);
    assert_eq!(value["isError"], false);
    let content = value["content"].as_array().expect("native content");
    assert_eq!(content.len(), 2);
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["mimeType"], "image/jpeg");
    assert_eq!(content[1]["data"], image_base64);
    assert_eq!(
        value["structuredContent"]["output"]["content_delivery"],
        "mcp_image"
    );
    assert_eq!(value["structuredContent"]["output"]["client_id"], "msi");
    assert!(value["structuredContent"]["output"]
        .get("content_base64")
        .is_none());
}

#[test]
fn project_connector_tools_list_is_exact_capability_registry() {
    let payload = project_connector_tools_list_payload_with_compact(false);
    let tools = payload["tools"].as_array().expect("tools array");
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, crate::connector_runtime::surface::CAPABILITY_NAMES);
    assert_eq!(tools.len(), 14);
    assert!(tools.iter().all(|tool| tool["inputSchema"].is_object()));
    assert!(tools.iter().all(|tool| tool["outputSchema"].is_object()));
    assert!(!names.contains(&"runtime_status"));
    assert!(!names.contains(&"list_projects"));
    assert!(!names.contains(&"start_session"));
    assert!(names.contains(&"code_navigate"));
    assert!(names.contains(&"code_impact"));
    for raw_name in [
        "lsp_status",
        "document_symbols",
        "workspace_symbols",
        "goto_definition",
        "find_references",
        "document_diagnostics",
        "hover",
        "call_hierarchy",
        "prepare_call_hierarchy",
        "incoming_calls",
        "outgoing_calls",
    ] {
        assert!(!names.contains(&raw_name));
    }
}

#[test]
fn mcp_tools_list_explicit_full_projection_retains_output_schema() {
    // Pure renderer with explicit compact=false. Exposure-specific defaults
    // are covered through the request adapter rather than inferred here.
    let value = mcp_tools_list_payload_with_compact(ModelSurface::FullOperatorRuntime, false);
    let tools = value["tools"].as_array().expect("tools array");
    assert!(!tools.is_empty());
    for tool in tools {
        assert!(tool["name"].is_string());
        assert!(tool["description"].is_string());
        assert!(tool["inputSchema"].is_object());
        assert!(
            tool["outputSchema"].is_object(),
            "explicit full projection must keep outputSchema for {}",
            tool["name"]
        );
        assert!(tool["annotations"].is_object() || tool.get("annotations").is_some());
    }
}

#[test]
fn retired_start_coding_task_is_absent_from_mcp_discovery() {
    let payload = mcp_tools_list_payload_with_compact(ModelSurface::FullOperatorRuntime, false);
    assert!(payload["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|tool| tool["name"] != "start_coding_task"));
}

#[test]
fn mcp_work_on_project_schema_exposes_managed_worktree_without_internal_operation() {
    let payload = mcp_tools_list_payload_with_compact(ModelSurface::FullOperatorRuntime, false);
    let tools = payload["tools"].as_array().expect("tools array");
    let work = tools
        .iter()
        .find(|tool| tool["name"] == "work_on_project")
        .expect("work_on_project MCP ToolSpec");
    let properties = work["inputSchema"]["properties"]
        .as_object()
        .expect("work_on_project input properties");
    assert_eq!(properties["mode"]["enum"], json!(["checkout", "worktree"]));
    assert_eq!(properties["mode"]["default"], "checkout");
    assert!(properties.contains_key("base_ref"));
    assert!(work["outputSchema"]["properties"]["output"]["properties"]["worktree"].is_object());
    assert!(tools
        .iter()
        .all(|tool| tool["name"] != "prepare_managed_worktree"));
}

#[test]
fn mcp_tools_list_compact_omits_output_schema_only() {
    // Pure renderer with the explicit compact=true switch; the env-adapter
    // path for compact mode is covered end-to-end by
    // `mcp_tools_list_returns_same_names_as_runtime`.
    let value = mcp_tools_list_payload_with_compact(ModelSurface::FullOperatorRuntime, true);
    let tools = value["tools"].as_array().expect("tools array");
    assert!(!tools.is_empty());
    for tool in tools {
        assert!(tool["name"].is_string(), "{tool:?}");
        assert!(tool["description"].is_string(), "{tool:?}");
        assert!(tool["inputSchema"].is_object(), "{tool:?}");
        assert!(
            tool.get("outputSchema").is_none(),
            "compact mode must omit outputSchema for {}",
            tool["name"]
        );
        // Compact projection deliberately preserves annotations; only outputSchema is omitted.
        assert!(
            tool.get("annotations").is_some(),
            "compact mode keeps annotations for {}",
            tool["name"]
        );
    }
}

#[test]
fn mcp_tools_list_compact_is_smaller_than_full_serialized() {
    // Explicit compact switches on the pure renderer: no env involvement.
    let full = serde_json::to_vec(&mcp_tools_list_payload_with_compact(
        ModelSurface::FullOperatorRuntime,
        false,
    ))
    .expect("full serialize");
    let compact = serde_json::to_vec(&mcp_tools_list_payload_with_compact(
        ModelSurface::FullOperatorRuntime,
        true,
    ))
    .expect("compact serialize");
    assert!(
        compact.len() < full.len(),
        "compact={} full={}",
        compact.len(),
        full.len()
    );
    // Guard against accidental total collapse (must still list many tools).
    assert!(
        compact.len() > 10_000,
        "compact unexpectedly tiny: {}",
        compact.len()
    );
}

// The compact switch is the tested product behavior: `tools/call` must be
// unaffected while `WEBCODEX_MCP_COMPACT_SCHEMAS` is set, so the env must stay
// stable (and serialized against other env-mutating tests) for the whole call.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn mcp_tools_call_still_returns_structured_content_under_compact_flag() {
    let mut env = crate::test_support::TestEnvGuard::new();
    env.set("WEBCODEX_MCP_COMPACT_SCHEMAS", "true");
    let runtime = test_runtime();
    let outcome = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(3)),
            json!({"name": "list_projects", "arguments": {}}),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(value) = outcome else {
        panic!("expected Ok, got {outcome:?}");
    };
    assert!(value["result"]["content"].is_array());
    assert!(value["result"]["structuredContent"].is_object());
    assert!(value["result"]["structuredContent"]["success"].is_boolean());
}

#[tokio::test]
async fn session_tools_exposed_in_registry_and_mcp() {
    // Session tools live on the full operator surface, not local_coding.
    // Assertions cover names/descriptions/inputSchema only, which compact
    // mode keeps, so no env or lock is needed.
    let runtime = test_runtime_with_surface(ModelSurface::FullOperatorRuntime);
    let specs = registered_tool_specs();
    let registry_names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
    assert!(registry_names.contains(&"session_summary"));
    assert!(registry_names.contains(&"update_session_context"));
    assert!(registry_names.contains(&"validation_summary"));
    for removed in [
        "bind_current_session",
        "current_session",
        "unbind_current_session",
    ] {
        assert!(
            !registry_names.contains(&removed),
            "removed Session tool leaked into registry: {removed}"
        );
    }
    assert!(!registry_names.contains(&"start_session"));

    let outcome = handle_mcp_request(
        &runtime,
        rpc("tools/list", Some(Value::from(31)), json!({})),
        None,
    )
    .await;
    let value = match outcome {
        McpOutcome::Ok(v) => v,
        other => panic!("expected Ok, got {:?}", other),
    };
    let names: Vec<String> = value["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.iter().any(|name| name == "session_summary"));
    assert!(names.iter().any(|name| name == "update_session_context"));
    assert!(names.iter().any(|name| name == "validation_summary"));
    assert!(!names.iter().any(|name| name == "start_session"));
    for removed in [
        "bind_current_session",
        "current_session",
        "unbind_current_session",
    ] {
        assert!(
            !names.iter().any(|name| name == removed),
            "removed Session tool leaked into MCP: {removed}"
        );
    }
    let tools = value["result"]["tools"].as_array().unwrap();
    let tool_description = |name: &str| {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing MCP tool {name}"))["description"]
            .as_str()
            .unwrap()
            .to_lowercase()
    };
    assert!(tool_description("session_summary").contains("session ledger"));
    assert!(tool_description("update_session_context").contains("authorized project"));
    assert!(tool_description("update_session_context").contains("background writer"));
    assert!(tool_description("update_session_context").contains("success does not mean"));
    assert!(tool_description("validation_summary").contains("does not run cargo"));
    assert!(tool_description("session_handoff_summary").contains("explicit session_id"));
    let validation_summary = tools
        .iter()
        .find(|tool| tool["name"] == "validation_summary")
        .expect("missing MCP validation_summary tool");
    assert_eq!(
        validation_summary["inputSchema"]["required"],
        json!(["project", "session_id"])
    );
    assert_eq!(
        validation_summary["inputSchema"]["additionalProperties"],
        false
    );
    for name in ["read_files", "run_shell", "write_project_file"] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing MCP tool {name}"));
        assert!(
            tool["inputSchema"]["properties"]
                .get("session_id")
                .is_some(),
            "MCP tools/list schema missing session_id for {name}"
        );
        assert!(
            !tool["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == "session_id"),
            "MCP tools/list must not require session_id for {name}"
        );
    }
}

#[tokio::test]
async fn mcp_tools_call_list_projects_returns_content_blocks() {
    let runtime = test_runtime();
    let outcome = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(Value::from(4)),
            json!({"name": "list_projects", "arguments": {}}),
        ),
        None,
    )
    .await;
    let value = match outcome {
        McpOutcome::Ok(v) => v,
        other => panic!("expected Ok, got {:?}", other),
    };
    assert_eq!(value["id"], 4);
    assert!(value["result"]["content"].is_array());
    assert_eq!(value["result"]["content"][0]["type"], "text");
    assert!(value["result"]["content"][0]["text"].is_string());
    assert!(value["result"]["structuredContent"].is_object());
    // No server-side project config is normal; without registered agents,
    // list_projects succeeds with an empty project array.
    assert_eq!(value["result"]["isError"], false);
}

#[tokio::test]
async fn mcp_tools_call_rejects_legacy_reserved_session_id_before_dispatch() {
    let runtime = test_runtime();
    let session = runtime.sessions.start_session(
        Some("demo".to_string()),
        Some("legacy recorder".to_string()),
    );
    let outcome = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(Value::from(32)),
            mcp_2026_params(json!({
                "name": "list_projects",
                "arguments": {"_session_id": &session.session_id}
            })),
        ),
        None,
    )
    .await;
    let value = match outcome {
        McpOutcome::BadRequest(value) => value,
        other => panic!("expected invalid-params BadRequest, got {other:?}"),
    };
    assert_eq!(value["error"]["code"], -32602);
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains("_session_id"));
    assert!(message.contains("no longer supported"));
    assert!(message.contains("recording_session_id"));
    assert_eq!(
        runtime
            .sessions
            .summary(&session.session_id, Some(10))
            .unwrap()
            .counts
            .tool_calls,
        0
    );
}

#[tokio::test]
async fn stateless_mcp_ack_wrapper_is_removed_before_concrete_dispatch_and_is_request_scoped() {
    let runtime = test_runtime();
    let session = runtime
        .sessions
        .start_session(None, Some("stateless ack wrapper".to_string()));
    let guidance = runtime
        .sessions
        .post_message_with_ack(
            crate::tool_runtime::sessions::PostSessionMessageInput {
                session_id: session.session_id.clone(),
                kind: crate::tool_runtime::sessions::SessionMessageKind::Guidance,
                message: "Remember this guidance for the current context.".to_string(),
                tags: Vec::new(),
                reply_to: None,
                priority: crate::tool_runtime::sessions::SessionMessagePriority::High,
            },
            true,
        )
        .unwrap();

    let call = |ack: Option<&str>, id: i64| {
        let mut arguments = json!({
            crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD: &session.session_id
        });
        if let Some(message_id) = ack {
            arguments[crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD] =
                json!([message_id, message_id]);
        }
        rpc(
            "tools/call",
            Some(Value::from(id)),
            mcp_2026_params(json!({"name": "list_projects", "arguments": arguments})),
        )
    };

    let acknowledged =
        handle_mcp_request(&runtime, call(Some(&guidance.message_id), 321), None).await;
    let acknowledged = match acknowledged {
        McpOutcome::Ok(value) => value,
        other => panic!("expected ACK call success, got {other:?}"),
    };
    assert_eq!(acknowledged["result"]["structuredContent"]["success"], true);
    assert_eq!(
        acknowledged["result"]["structuredContent"]["output"]["session_attention"]["ack"]
            ["accepted_count"],
        1
    );
    assert!(
        acknowledged["result"]["structuredContent"]["output"]["session_attention"]["messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let forgotten = handle_mcp_request(&runtime, call(None, 322), None).await;
    let forgotten = match forgotten {
        McpOutcome::Ok(value) => value,
        other => panic!("expected forgotten-ACK call success, got {other:?}"),
    };
    assert_eq!(forgotten["result"]["structuredContent"]["success"], true);
    assert_eq!(
        forgotten["result"]["structuredContent"]["output"]["session_attention"]["messages"][0]
            ["message_id"],
        guidance.message_id
    );
    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    let started = summary
        .events
        .iter()
        .find(|event| event.kind == "tool_call_started")
        .unwrap();
    let input = serde_json::to_string(&started.input_summary).unwrap();
    assert!(!input.contains("ack_session_message_ids"));
    assert!(!input.contains("__webcodex_stateless_ack_session_message_ids"));
}

#[tokio::test]
async fn mcp_tools_call_rejects_legacy_session_alias_even_with_canonical_recorder() {
    let runtime = test_runtime();
    let canonical = runtime
        .sessions
        .start_session(None, Some("canonical recorder".to_string()));

    let outcome = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(Value::from(320)),
            mcp_2026_params(json!({
                "name": "list_projects",
                "arguments": {
                    crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD: &canonical.session_id,
                    "_session_id": &canonical.session_id
                }
            })),
        ),
        None,
    )
    .await;
    let value = match outcome {
        McpOutcome::BadRequest(value) => value,
        other => panic!("expected invalid-params BadRequest, got {other:?}"),
    };
    assert_eq!(value["error"]["code"], -32602);
    assert!(value["error"]["message"]
        .as_str()
        .is_some_and(
            |message| message.contains("_session_id") && message.contains("no longer supported")
        ));
    assert_eq!(
        runtime
            .sessions
            .summary(&canonical.session_id, Some(10))
            .unwrap()
            .counts
            .tool_calls,
        0
    );
}

#[tokio::test]
async fn mcp_tools_call_records_event_with_recording_session_id() {
    let runtime = test_runtime();
    let session = runtime.sessions.start_session(None, None);
    let outcome = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(Value::from(33)),
            mcp_2026_params(json!({
                "name": "list_projects",
                "arguments": {
                    crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD: &session.session_id
                }
            })),
        ),
        None,
    )
    .await;
    match outcome {
        McpOutcome::Ok(value) => {
            assert_eq!(value["result"]["structuredContent"]["success"], true);
        }
        other => panic!("expected Ok, got {:?}", other),
    }
    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(10))
        .unwrap();
    assert_eq!(summary.counts.tool_calls, 1);
    assert_eq!(summary.counts.succeeded, 1);
    let finished = summary
        .events
        .iter()
        .find(|event| event.kind == "tool_call_finished")
        .unwrap();
    assert_eq!(finished.transport, "mcp");
    assert_eq!(finished.status.as_deref(), Some("succeeded"));
    assert_eq!(finished.context_revision, None);
    assert_eq!(
        runtime.sessions.context_revision(&session.session_id),
        Some(0)
    );
    assert_eq!(finished.risk_class, "read_only");
}

#[tokio::test]
async fn mcp_tools_list_hides_testing_metadata_while_raw_call_records_it() {
    let runtime = test_runtime();
    let listed = handle_mcp_request(
        &runtime,
        rpc("tools/list", Some(Value::from(330)), json!({})),
        None,
    )
    .await;
    let listed = match listed {
        McpOutcome::Ok(value) => value,
        other => panic!("expected tools/list Ok, got {other:?}"),
    };
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert!(
        tools.iter().all(|tool| tool["name"] != "job_status"),
        "retired job_status must stay absent from MCP tools/list"
    );
    let observe_jobs = tools
        .iter()
        .find(|tool| tool["name"] == "observe_jobs")
        .expect("observe_jobs must be model-visible on local_coding");
    let properties = observe_jobs["inputSchema"]["properties"]
        .as_object()
        .unwrap();
    for field in [
        "expected_failure",
        "expected_failure_kind",
        "assertion_name",
    ] {
        assert!(
            !properties.contains_key(field),
            "MCP tools/list must not publish recorder metadata field {field}"
        );
    }

    let session = runtime
        .sessions
        .start_session(None, Some("hidden metadata compatibility".to_string()));
    let outcome = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(Value::from(331)),
            mcp_2026_params(json!({
                "name": "stop_job",
                "arguments": {
                    crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD: &session.session_id,
                    "project": "agent:nope:nope",
                    "job_id": "missing-job",
                    "confirm": false,
                    "expected_failure": true,
                    "expected_failure_kind": "confirmation_required",
                    "assertion_name": "mcp hidden metadata compatibility"
                }
            })),
        ),
        None,
    )
    .await;
    let value = match outcome {
        McpOutcome::Ok(value) => value,
        other => panic!("expected tools/call result, got {other:?}"),
    };
    assert_eq!(value["result"]["isError"], true);

    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(10))
        .unwrap();
    let finished = summary
        .events
        .iter()
        .find(|event| event.kind == "tool_call_finished")
        .expect("raw MCP call must be recorded");
    assert_eq!(finished.tool_name, "stop_job");
    assert_eq!(finished.expected_failure, Some(true));
    assert_eq!(
        finished.expected_failure_kind.as_deref(),
        Some("confirmation_required")
    );
    assert_eq!(
        finished.assertion_name.as_deref(),
        Some("mcp hidden metadata compatibility")
    );
    assert_eq!(
        finished.actual_failure_kind.as_deref(),
        Some("confirmation_required")
    );
    assert_eq!(
        finished.failure_expectation_result.as_deref(),
        Some("matched_expected_failure")
    );
}

#[tokio::test]
async fn mcp_show_changes_distinguishes_recording_session_id_from_query_session_id() {
    use crate::runner_protocol::{
        RunnerCapabilities, RunnerProjectSummary, RunnerRegisterRequest, RunnerResultRequest,
    };

    let runtime = test_runtime();
    runtime
        .runner_registry
        .register(crate::test_support::current_runner_registration(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: "mcp-client".to_string(),
                runner_instance_id: "inst".to_string(),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: None,
                owner: None,
                hostname: None,
                host_context: None,
                capabilities: RunnerCapabilities {
                    shell: true,
                    git: true,
                    internal_posix_script: true,
                    ..Default::default()
                },
                policy: None,
            },
        ))
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        "mcp-client",
        "inst",
        vec![RunnerProjectSummary {
            id: "demo".to_string(),
            name: Some("demo".to_string()),
            path: "/tmp/mcp-demo".to_string(),
            allow_patch: true,
            kind: None,
            registration_source: None,
            description: None,
            hooks: Vec::new(),
            disabled: false,
            revision: None,
            git_branch: None,
            git_head: None,
            git_dirty: None,
            updated_at: chrono::Utc::now().timestamp(),
            shell_profile: None,
        }],
    )
    .await;
    let project = "agent:mcp-client:demo";
    let tracking_session = runtime
        .sessions
        .start_session(Some(project.to_string()), Some("track call".to_string()));
    let query_session = runtime
        .sessions
        .start_session(Some(project.to_string()), Some("query session".to_string()));
    let write_args = json!({"project": project, "path": "src/query.rs"});
    let start = runtime.sessions.record_tool_call_started(
        Some(&query_session.session_id),
        crate::tool_runtime::sessions::SessionTransport::Mcp,
        "write_project_file",
        &write_args,
        crate::tool_runtime::sessions::session_tool_contract("write_project_file"),
    );
    runtime
        .sessions
        .record_tool_call_finished(start, true, &json!({}), None, None);
    let auth = AuthContext {
        role: Some("admin".to_string()),
        scopes: vec!["admin".to_string()],
        is_bootstrap: true,
        ..AuthContext::new(crate::auth::AuthKind::Bootstrap)
    };

    let outcome = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(Value::from(34)),
            mcp_2026_params(json!({
                "name": "show_changes",
                "arguments": {
                    crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD: &tracking_session.session_id,
                    "project": project,
                    "session_id": &query_session.session_id,
                    "include_diff": false
                }
            })),
        ),
        Some(&auth),
    );
    let complete = async {
        let req = wait_for_mcp_agent_request(
            &runtime.runner_registry,
            "mcp-client",
            "inst",
            "show_changes",
        )
        .await;
        let stdout = crate::tool_runtime::framed_clean_show_changes_test_stdout("test head", false);
        runtime
            .runner_registry
            .complete(RunnerResultRequest {
                client_id: "mcp-client".to_string(),
                runner_instance_id: "inst".to_string(),
                request_id: req.request_id,
                exit_code: Some(0),
                stdout: Some(stdout),
                stderr: Some(String::new()),
                duration_ms: Some(1),
                error: None,
            })
            .await
            .unwrap();
    };
    let (outcome, _) = tokio::join!(outcome, complete);
    let value = match outcome {
        McpOutcome::Ok(value) => value,
        other => panic!("expected Ok, got {:?}", other),
    };
    let output = &value["result"]["structuredContent"]["output"];
    assert_eq!(output["session"]["found"], true);
    assert_eq!(output["session"]["session_id"], query_session.session_id);
    assert_eq!(output["session"]["changed_paths"], json!(["src/query.rs"]));

    let tracking_summary = runtime
        .sessions
        .summary(&tracking_session.session_id, Some(10))
        .unwrap();
    assert!(tracking_summary
        .events
        .iter()
        .any(|event| event.tool_name == "show_changes"));
}

#[tokio::test]
async fn mcp_tools_call_unknown_tool_is_bad_request() {
    let runtime = test_runtime();
    let outcome = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(Value::from(5)),
            json!({"name": "no_such_tool", "arguments": {}}),
        ),
        None,
    )
    .await;
    match outcome {
        McpOutcome::BadRequest(value) => {
            assert_eq!(value["error"]["code"], -32602);
            assert!(value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no_such_tool"));
        }
        other => panic!("expected BadRequest, got {:?}", other),
    }
}
