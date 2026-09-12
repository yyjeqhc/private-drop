use super::support::*;
use crate::lsp_bridge::{
    error_codes, parse_runner_lsp_result_envelope, CallHierarchyDirection,
    CallHierarchyEdgeDirection, CallHierarchyResult, DocumentDiagnosticsResult,
    DocumentDiagnosticsStatus, DocumentSymbolsResult, HoverResult, LocationsResult,
    LspAvailabilityStatus, LspStatusResult, PublicCallHierarchyEdge, PublicCallHierarchySymbol,
    PublicDiagnostic, PublicHover, PublicLocation, PublicPosition, PublicRange, PublicSymbol,
    PublicWorkspaceSymbol, RunnerLspPayload, RunnerLspRequest, RunnerLspResultEnvelope,
    WorkspaceSymbolsResult, AGENT_LSP_REQUEST_KIND,
};
use crate::runner_protocol::{RunnerCapabilities, RunnerRegisterRequest};
use crate::tool_runtime::tool_definition::{
    lookup_tool_definition, model_visible_tool_definitions, RunnerCapabilityRequirement,
    TOOL_CATEGORY_LSP,
};
use crate::tool_runtime::{ToolCall, ToolResult};
use serde_json::{json, Value};

#[test]
fn lsp_tools_are_registered_read_only_and_not_shell_like() {
    for name in [
        "lsp_status",
        "document_symbols",
        "document_diagnostics",
        "hover",
        "workspace_symbols",
        "goto_definition",
        "find_references",
    ] {
        let def = lookup_tool_definition(name).expect(name);
        assert_eq!(def.category, TOOL_CATEGORY_LSP, "{name}");
        assert_eq!(
            def.metadata.effect,
            crate::tool_runtime::metadata::ToolEffect::Observe,
            "{name}"
        );
        assert!(!def.metadata.destructive, "{name}");
        assert!(!def.metadata.shell_like, "{name}");
        assert_eq!(
            def.runner_capability,
            Some(RunnerCapabilityRequirement::LspReadOnlyNavigation),
            "{name}"
        );
        assert_eq!(
            def.metadata.authority,
            crate::tool_runtime::metadata::ToolAuthorityPolicy::Require(
                crate::tool_runtime::metadata::PROJECT_READ
            ),
            "{name}"
        );
    }
    let hierarchy = lookup_tool_definition("call_hierarchy").expect("call_hierarchy");
    assert_eq!(hierarchy.category, TOOL_CATEGORY_LSP);
    assert_eq!(
        hierarchy.metadata.effect,
        crate::tool_runtime::metadata::ToolEffect::Observe
    );
    assert_eq!(
        hierarchy.runner_capability,
        Some(RunnerCapabilityRequirement::LspCallHierarchy)
    );
    assert_eq!(
        hierarchy.metadata.authority,
        crate::tool_runtime::metadata::ToolAuthorityPolicy::Require(
            crate::tool_runtime::metadata::PROJECT_READ
        )
    );
    let names: Vec<_> = model_visible_tool_definitions().map(|d| d.name).collect();
    for name in [
        "lsp_status",
        "document_symbols",
        "document_diagnostics",
        "hover",
        "workspace_symbols",
        "goto_definition",
        "find_references",
        "call_hierarchy",
    ] {
        assert!(names.contains(&name), "missing {name} in known tools");
    }
}

#[test]
fn typed_lsp_session_audit_keeps_paths_but_never_symbol_queries() {
    let definition = ToolCall::GotoDefinition {
        project: "agent:test:demo".to_string(),
        path: "src/caller.rs".to_string(),
        line: 4,
        column: 8,
        limit: Some(20),
        session_id: Some("wc_sess_test".to_string()),
    }
    .session_log_arguments();
    assert_eq!(definition["path"], "src/caller.rs");
    assert_eq!(definition["line"], 4);
    assert_eq!(definition["column"], 8);

    let symbols = ToolCall::WorkspaceSymbols {
        project: "agent:test:demo".to_string(),
        query: "PRIVATE_SYMBOL_QUERY".to_string(),
        limit: Some(10),
        session_id: Some("wc_sess_test".to_string()),
    }
    .session_log_arguments();
    assert_eq!(symbols["query_present"], true);
    assert!(
        !symbols.to_string().contains("PRIVATE_SYMBOL_QUERY"),
        "workspace symbol query must not enter the session ledger"
    );
}

#[test]
fn lsp_input_schemas_have_required_bounds() {
    use crate::tool_runtime::registry::registered_tool_specs;
    let specs = registered_tool_specs();
    let by_name: std::collections::HashMap<_, _> =
        specs.into_iter().map(|s| (s.name.clone(), s)).collect();

    let status = &by_name["lsp_status"].input_schema;
    assert_eq!(status["required"], json!(["project"]));
    assert_eq!(status["additionalProperties"], false);

    let symbols = &by_name["document_symbols"].input_schema;
    assert_eq!(symbols["required"], json!(["project", "path"]));
    assert!(symbols["properties"]["limit"].get("maximum").is_none());
    assert_eq!(symbols["additionalProperties"], false);

    let diagnostics = &by_name["document_diagnostics"].input_schema;
    assert_eq!(diagnostics["required"], json!(["project", "path"]));
    assert_eq!(diagnostics["properties"]["limit"]["minimum"], 1);
    assert!(diagnostics["properties"]["limit"].get("maximum").is_none());
    assert_eq!(diagnostics["properties"]["limit"]["default"], 100);
    assert_eq!(diagnostics["additionalProperties"], false);
    let hierarchy = &by_name["call_hierarchy"].input_schema;
    assert_eq!(
        hierarchy["required"],
        json!(["project", "path", "line", "column"])
    );
    assert_eq!(
        hierarchy["properties"]["direction"]["enum"],
        json!(["incoming", "outgoing", "both"])
    );
    assert_eq!(hierarchy["properties"]["depth"]["maximum"], 2);
    assert_eq!(hierarchy["properties"]["limit"]["maximum"], 100);
    assert_eq!(hierarchy["additionalProperties"], false);
    let diagnostics_output = &by_name["document_diagnostics"].output_schema;
    let output_properties = &diagnostics_output["properties"]["output"]["properties"];
    for field in [
        "project",
        "path",
        "language",
        "diagnostics",
        "total_count",
        "returned_count",
        "truncated",
        "status",
        "clean",
        "published_version",
        "invalid_results_omitted",
        "related_information_omitted",
    ] {
        assert!(
            output_properties.get(field).is_some(),
            "diagnostics output schema missing {field}"
        );
    }
    assert!(output_properties.get("fresh").is_none());
    assert!(output_properties.get("timed_out").is_none());
    let diagnostic_item = &output_properties["diagnostics"]["items"];
    assert_eq!(diagnostic_item["additionalProperties"], false);
    assert_eq!(diagnostic_item["properties"]["message"]["maxLength"], 4096);
    assert!(diagnostic_item["properties"].get("data").is_none());
    assert!(diagnostic_item["properties"]
        .get("relatedInformation")
        .is_none());

    let hover = &by_name["hover"].input_schema;
    assert_eq!(
        hover["required"],
        json!(["project", "path", "line", "column"])
    );
    assert_eq!(hover["properties"]["line"]["minimum"], 1);
    assert_eq!(hover["properties"]["column"]["minimum"], 1);
    assert_eq!(hover["additionalProperties"], false);
    let hover_output = &by_name["hover"].output_schema["properties"]["output"]["properties"];
    assert_eq!(
        hover_output["hover"]["anyOf"][0]["properties"]["value"]["maxLength"],
        16384
    );

    let workspace = &by_name["workspace_symbols"].input_schema;
    assert_eq!(workspace["required"], json!(["project", "query"]));
    assert_eq!(workspace["properties"]["query"]["minLength"], 1);
    assert_eq!(workspace["properties"]["query"]["maxLength"], 200);
    assert_eq!(workspace["properties"]["limit"]["default"], 50);
    assert!(workspace["properties"]["limit"].get("maximum").is_none());
    assert_eq!(workspace["additionalProperties"], false);
    let workspace_item = &by_name["workspace_symbols"].output_schema["properties"]["output"]
        ["properties"]["symbols"]["items"];
    assert_eq!(workspace_item["additionalProperties"], false);
    assert!(workspace_item["properties"].get("uri").is_none());
    assert!(workspace_item["properties"].get("data").is_none());

    let goto = &by_name["goto_definition"].input_schema;
    assert_eq!(
        goto["required"],
        json!(["project", "path", "line", "column"])
    );
    assert_eq!(goto["properties"]["line"]["minimum"], 1);
    assert_eq!(goto["properties"]["column"]["minimum"], 1);
    assert!(goto["properties"]["limit"].get("maximum").is_none());

    let refs = &by_name["find_references"].input_schema;
    assert_eq!(
        refs["required"],
        json!(["project", "path", "line", "column"])
    );
    assert_eq!(refs["properties"]["include_declaration"]["default"], true);
    assert!(refs["properties"]["limit"].get("maximum").is_none());
    assert_eq!(refs["additionalProperties"], false);

    // Flattened Action fields must list path/line/column/include_declaration/limit.
    use crate::tool_runtime::accepted_flattened_args_for_spec;
    let flat_goto = accepted_flattened_args_for_spec(&by_name["goto_definition"]);
    for field in ["project", "path", "line", "column", "limit", "session_id"] {
        assert!(
            flat_goto.iter().any(|f| f == field),
            "goto missing flattened {field}: {flat_goto:?}"
        );
    }
    let flat_refs = accepted_flattened_args_for_spec(&by_name["find_references"]);
    for field in [
        "project",
        "path",
        "line",
        "column",
        "include_declaration",
        "limit",
        "session_id",
    ] {
        assert!(
            flat_refs.iter().any(|f| f == field),
            "refs missing flattened {field}: {flat_refs:?}"
        );
    }
    let flat_diagnostics = accepted_flattened_args_for_spec(&by_name["document_diagnostics"]);
    for field in ["project", "path", "limit", "session_id"] {
        assert!(
            flat_diagnostics.iter().any(|item| item == field),
            "diagnostics missing flattened {field}: {flat_diagnostics:?}"
        );
    }
    let flat_hover = accepted_flattened_args_for_spec(&by_name["hover"]);
    for field in ["project", "path", "line", "column", "session_id"] {
        assert!(flat_hover.iter().any(|item| item == field));
    }
    let flat_workspace = accepted_flattened_args_for_spec(&by_name["workspace_symbols"]);
    for field in ["project", "query", "limit", "session_id"] {
        assert!(flat_workspace.iter().any(|item| item == field));
    }
}

#[test]
fn call_hierarchy_parser_rejects_explicit_null_optional_fields() {
    for field in ["direction", "depth", "limit"] {
        let mut arguments = json!({
            "project": "agent:oe:demo",
            "path": "src/main.rs",
            "line": 1,
            "column": 1
        });
        arguments[field] = Value::Null;
        let error = ToolCall::from_tool_name("call_hierarchy", arguments).unwrap_err();
        assert!(
            error.contains("invalid arguments for tool 'call_hierarchy'"),
            "{field}: {error}"
        );
    }
}

#[test]
fn document_diagnostics_tool_call_parser_produces_only_typed_fields() {
    let call = ToolCall::from_tool_name(
        "document_diagnostics",
        json!({
            "project": "agent:oe:demo",
            "path": "src/main.rs",
            "limit": 25,
            "session_id": "wc_sess_demo"
        }),
    )
    .unwrap();
    assert!(matches!(
        call,
        ToolCall::DocumentDiagnostics {
            project,
            path,
            limit: Some(25),
            session_id: Some(session_id),
        } if project == "agent:oe:demo"
            && path == "src/main.rs"
            && session_id == "wc_sess_demo"
    ));
    let call_with_ignored_internal_extra = ToolCall::from_tool_name(
        "document_diagnostics",
        json!({"project": "agent:oe:demo", "path": "src/main.rs", "timeout": 30}),
    )
    .unwrap();
    assert!(matches!(
        call_with_ignored_internal_extra,
        ToolCall::DocumentDiagnostics {
            limit: None,
            session_id: None,
            ..
        }
    ));
}

async fn register_lsp_agent(
    runtime: &crate::tool_runtime::ToolRuntime,
    client_id: &str,
    project_id: &str,
    root: &std::path::Path,
    lsp_capable: bool,
) -> String {
    register_lsp_agent_capabilities(
        runtime,
        client_id,
        project_id,
        root,
        lsp_capable,
        lsp_capable,
    )
    .await
}

async fn register_lsp_agent_capabilities(
    runtime: &crate::tool_runtime::ToolRuntime,
    client_id: &str,
    project_id: &str,
    root: &std::path::Path,
    lsp_capable: bool,
    call_hierarchy_capable: bool,
) -> String {
    let project_path = root.to_string_lossy().to_string();
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
                client_id: client_id.to_string(),
                runner_instance_id: "inst".to_string(),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: None,
                owner: None,
                hostname: None,
                host_context: None,
                capabilities: RunnerCapabilities {
                    shell: true,
                    file_read: true,
                    file_write: true,
                    lsp_read_only_navigation: lsp_capable,
                    lsp_call_hierarchy: call_hierarchy_capable,
                    ..Default::default()
                },
                policy: None,
            },
        ))
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        "inst",
        vec![registered_project(project_id, &project_path)],
    )
    .await;
    crate::tool_runtime::runner_project_runtime_id(client_id, project_id)
}

async fn complete_lsp_agent_request(
    runtime: &crate::tool_runtime::ToolRuntime,
    client_id: &str,
    result: impl serde::Serialize,
) {
    let req = wait_for_patch_agent_request(runtime, client_id).await;
    assert_eq!(req.kind, AGENT_LSP_REQUEST_KIND);
    assert!(req.lsp.is_some());
    assert!(req.command.is_empty());
    let envelope = RunnerLspResultEnvelope::ok(result);
    complete_patch_agent_request(
        runtime,
        client_id,
        &req.request_id,
        0,
        &envelope.to_stdout_json(),
        "",
    )
    .await;
}

fn document_symbols_result(path: &str) -> DocumentSymbolsResult {
    DocumentSymbolsResult {
        project: "demo".into(),
        path: path.into(),
        language: "rust".into(),
        symbols: vec![],
        total_count: 0,
        returned_count: 0,
        truncated: false,
        external_results_omitted: 0,
        invalid_results_omitted: 0,
    }
}

fn document_diagnostics_result(path: &str) -> DocumentDiagnosticsResult {
    DocumentDiagnosticsResult {
        project: "demo".into(),
        path: path.into(),
        language: "rust".into(),
        diagnostics: vec![PublicDiagnostic {
            range: PublicRange {
                start: PublicPosition { line: 1, column: 1 },
                end: PublicPosition { line: 1, column: 2 },
            },
            severity: "warning".into(),
            severity_code: Some(2),
            code: Some("unused".into()),
            source: Some("rust-analyzer".into()),
            message: "unused item".into(),
            tags: vec!["unnecessary".into()],
        }],
        total_count: 1,
        returned_count: 1,
        truncated: false,
        status: DocumentDiagnosticsStatus::Complete,
        clean: Some(false),
        published_version: Some(2),
        invalid_results_omitted: 0,
        related_information_omitted: 0,
    }
}

fn hover_result(path: &str) -> HoverResult {
    HoverResult {
        project: "demo".into(),
        path: path.into(),
        position: PublicPosition { line: 1, column: 1 },
        hover: Some(PublicHover {
            kind: "markdown".into(),
            value: "`main`".into(),
            range: None,
        }),
        truncated: false,
        range_omitted: false,
    }
}

fn workspace_symbols_result() -> WorkspaceSymbolsResult {
    WorkspaceSymbolsResult {
        project: "demo".into(),
        query: "ToolRuntime".into(),
        symbols: vec![PublicWorkspaceSymbol {
            name: "ToolRuntime".into(),
            kind: "struct".into(),
            kind_code: 23,
            container_name: None,
            path: "src/tool_runtime/mod.rs".into(),
            range: None,
        }],
        total_results: 1,
        returned_count: 1,
        truncated: false,
        external_results_omitted: 0,
        invalid_results_omitted: 0,
    }
}

fn call_hierarchy_result(path: &str) -> CallHierarchyResult {
    let range = PublicRange {
        start: PublicPosition { line: 1, column: 1 },
        end: PublicPosition { line: 1, column: 5 },
    };
    CallHierarchyResult {
        project: "demo".into(),
        path: path.into(),
        language: "rust".into(),
        query_position: PublicPosition { line: 1, column: 4 },
        direction: CallHierarchyDirection::Both,
        depth: 1,
        roots: vec![PublicCallHierarchySymbol {
            name: "root".into(),
            kind: "function".into(),
            kind_code: 12,
            path: path.into(),
            range: range.clone(),
            selection_range: range,
        }],
        root_total_count: 1,
        root_returned_count: 1,
        edges: vec![],
        returned_count: 0,
        truncated: false,
        external_results_omitted: 0,
        invalid_results_omitted: 0,
        call_site_ranges_omitted: 0,
    }
}

fn call_hierarchy_result_with_edge(path: &str) -> CallHierarchyResult {
    let mut result = call_hierarchy_result(path);
    let mut caller = result.roots[0].clone();
    caller.name = "caller".into();
    caller.path = "src/caller.rs".into();
    let call_site = result.roots[0].selection_range.clone();
    result.edges.push(PublicCallHierarchyEdge {
        direction: CallHierarchyEdgeDirection::Incoming,
        depth: 1,
        from: caller,
        to: result.roots[0].clone(),
        call_sites: vec![call_site],
    });
    result.returned_count = 1;
    result
}

async fn dispatch_call_hierarchy_result(
    client_id: &str,
    result: CallHierarchyResult,
) -> ToolResult {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let project = register_lsp_agent(&runtime, client_id, "demo", tmp.path(), true).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::CallHierarchy {
                        project,
                        path: "src/main.rs".into(),
                        line: 1,
                        column: 4,
                        direction: CallHierarchyDirection::Both,
                        depth: 1,
                        limit: 50,
                        session_id: None,
                    },
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    complete_lsp_agent_request(&runtime, client_id, result).await;
    task.await.unwrap()
}

fn assert_malformed_call_hierarchy(case: &str, result: &ToolResult) {
    assert!(!result.success, "{case}: {result:?}");
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains(error_codes::MALFORMED_AGENT_LSP_RESULT),
        "{case}: {result:?}"
    );
}

async fn dispatch_document_symbols_with_result_path(client_id: &str, path: &str) -> ToolResult {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let project = register_lsp_agent(&runtime, client_id, "demo", tmp.path(), true).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::DocumentSymbols {
                        project,
                        path: "src/main.rs".into(),
                        limit: Some(10),
                        session_id: None,
                    },
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    complete_lsp_agent_request(&runtime, client_id, document_symbols_result(path)).await;
    task.await.unwrap()
}

#[tokio::test]
async fn call_hierarchy_does_not_require_legacy_navigation_capability() {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let project = register_lsp_agent_capabilities(
        &runtime,
        "hierarchy-only",
        "demo",
        tmp.path(),
        false,
        true,
    )
    .await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::CallHierarchy {
                        project,
                        path: "src/main.rs".into(),
                        line: 1,
                        column: 4,
                        direction: CallHierarchyDirection::Both,
                        depth: 1,
                        limit: 50,
                        session_id: None,
                    },
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    complete_lsp_agent_request(
        &runtime,
        "hierarchy-only",
        call_hierarchy_result("src/main.rs"),
    )
    .await;
    let result = task.await.unwrap();
    assert!(
        result.success,
        "call_hierarchy must depend only on its distinct capability: {result:?}"
    );
}

#[tokio::test]
async fn call_hierarchy_dispatch_uses_typed_bridge_and_validates_bounds() {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let project = register_lsp_agent(&runtime, "hierarchy-agent", "demo", tmp.path(), true).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::CallHierarchy {
                        project,
                        path: "src/main.rs".into(),
                        line: 1,
                        column: 4,
                        direction: CallHierarchyDirection::Both,
                        depth: 1,
                        limit: 50,
                        session_id: None,
                    },
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "hierarchy-agent").await;
    assert_eq!(
        request.lsp.as_ref().map(|payload| &payload.request),
        Some(&RunnerLspRequest::CallHierarchy {
            path: "src/main.rs".into(),
            line: 1,
            column: 4,
            direction: CallHierarchyDirection::Both,
            depth: 1,
            limit: 50,
        })
    );
    let envelope = RunnerLspResultEnvelope::ok(call_hierarchy_result("src/main.rs"));
    complete_patch_agent_request(
        &runtime,
        "hierarchy-agent",
        &request.request_id,
        0,
        &envelope.to_stdout_json(),
        "",
    )
    .await;
    let result = task.await.unwrap();
    assert!(result.success, "{result:?}");
    assert_eq!(result.output["project"], project);

    for (depth, limit) in [(0, 50), (1, 101)] {
        let invalid = runtime
            .dispatch_with_auth(
                ToolCall::CallHierarchy {
                    project: project.clone(),
                    path: "src/main.rs".into(),
                    line: 1,
                    column: 4,
                    direction: CallHierarchyDirection::Both,
                    depth,
                    limit,
                    session_id: None,
                },
                Some(&auth_context(None, true)),
            )
            .await;
        assert!(!invalid.success, "{depth}/{limit}: {invalid:?}");
    }
}

#[tokio::test]
async fn call_hierarchy_result_boundary_rejects_inconsistent_bounds() {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let project =
        register_lsp_agent(&runtime, "hierarchy-malformed", "demo", tmp.path(), true).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::CallHierarchy {
                        project,
                        path: "src/main.rs".into(),
                        line: 1,
                        column: 4,
                        direction: CallHierarchyDirection::Both,
                        depth: 1,
                        limit: 50,
                        session_id: None,
                    },
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    let mut malformed = call_hierarchy_result("src/main.rs");
    malformed.returned_count = 1;
    complete_lsp_agent_request(&runtime, "hierarchy-malformed", malformed).await;
    let result = task.await.unwrap();
    assert!(!result.success, "{result:?}");
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains(error_codes::MALFORMED_AGENT_LSP_RESULT),
        "{result:?}"
    );
}

#[tokio::test]
async fn call_hierarchy_result_boundary_rejects_cross_field_correlation_mismatches() {
    let mut cases = Vec::new();

    let mut wrong_direction = call_hierarchy_result("src/main.rs");
    wrong_direction.direction = CallHierarchyDirection::Incoming;
    cases.push(("direction", wrong_direction));

    let mut wrong_depth = call_hierarchy_result("src/main.rs");
    wrong_depth.depth = 2;
    cases.push(("depth", wrong_depth));

    let mut wrong_query_position = call_hierarchy_result("src/main.rs");
    wrong_query_position.query_position.column = 5;
    cases.push(("query-position", wrong_query_position));

    let mut edge_too_deep = call_hierarchy_result_with_edge("src/main.rs");
    edge_too_deep.edges[0].depth = 2;
    cases.push(("edge-depth", edge_too_deep));

    for (name, malformed) in cases {
        let result = dispatch_call_hierarchy_result("hierarchy-correlation", malformed).await;
        assert_malformed_call_hierarchy(name, &result);
    }
}

#[tokio::test]
async fn call_hierarchy_result_boundary_rejects_public_symbol_and_range_violations() {
    let mut cases = Vec::new();

    let mut zero_based = call_hierarchy_result("src/main.rs");
    zero_based.roots[0].range.start.line = 0;
    cases.push(("zero-based", zero_based));

    let mut reversed_range = call_hierarchy_result("src/main.rs");
    reversed_range.roots[0].selection_range.start.column = 5;
    reversed_range.roots[0].selection_range.end.column = 1;
    cases.push(("reversed-range", reversed_range));

    let mut oversized_name = call_hierarchy_result("src/main.rs");
    oversized_name.roots[0].name = "x".repeat(257);
    cases.push(("oversized-name", oversized_name));

    let mut empty_kind = call_hierarchy_result("src/main.rs");
    empty_kind.roots[0].kind.clear();
    cases.push(("empty-kind", empty_kind));

    let mut reversed_call_site = call_hierarchy_result_with_edge("src/main.rs");
    reversed_call_site.edges[0].call_sites[0].start.column = 5;
    reversed_call_site.edges[0].call_sites[0].end.column = 1;
    cases.push(("reversed-call-site", reversed_call_site));

    for (name, malformed) in cases {
        let result = dispatch_call_hierarchy_result("hierarchy-public-bounds", malformed).await;
        assert_malformed_call_hierarchy(name, &result);
    }
}

#[tokio::test]
async fn call_hierarchy_result_boundary_rejects_request_path_mismatch() {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let project = register_lsp_agent(
        &runtime,
        "hierarchy-path-mismatch",
        "demo",
        tmp.path(),
        true,
    )
    .await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::CallHierarchy {
                        project,
                        path: "src/./main.rs".into(),
                        line: 1,
                        column: 4,
                        direction: CallHierarchyDirection::Both,
                        depth: 1,
                        limit: 50,
                        session_id: None,
                    },
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    let malformed = call_hierarchy_result("src/other.rs");
    complete_lsp_agent_request(&runtime, "hierarchy-path-mismatch", malformed).await;
    let result = task.await.unwrap();
    assert!(!result.success, "{result:?}");
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains(error_codes::MALFORMED_AGENT_LSP_RESULT),
        "{result:?}"
    );
}

#[tokio::test]
async fn call_hierarchy_result_symbol_paths_must_be_normalized_project_relative() {
    for path in [
        "../outside.rs",
        "src/../../outside.rs",
        "/tmp/outside.rs",
        "file:///tmp/outside.rs",
        "C:/tmp/outside.rs",
        "C:outside.rs",
        r"\\server\share\outside.rs",
        "//server/share/outside.rs",
        r"src\module\service.rs",
        "./src/main.rs",
        "src/./main.rs",
        "src//main.rs",
        "",
    ] {
        let mut malformed = call_hierarchy_result("src/main.rs");
        malformed.roots[0].path = path.to_string();
        let result = dispatch_call_hierarchy_result("hierarchy-symbol-path", malformed).await;
        assert_malformed_call_hierarchy(path, &result);
    }

    let mut parent_from = call_hierarchy_result_with_edge("src/main.rs");
    parent_from.edges[0].from.path = "../caller.rs".into();
    let result = dispatch_call_hierarchy_result("hierarchy-edge-from-path", parent_from).await;
    assert_malformed_call_hierarchy("edge-from", &result);

    let mut parent_to = call_hierarchy_result_with_edge("src/main.rs");
    parent_to.edges[0].to.path = "src/../../callee.rs".into();
    let result = dispatch_call_hierarchy_result("hierarchy-edge-to-path", parent_to).await;
    assert_malformed_call_hierarchy("edge-to", &result);

    let mut valid = call_hierarchy_result_with_edge("src/main.rs");
    valid.roots[0].path = "src/module/service.rs".into();
    valid.edges[0].from.path = "src/module/caller.rs".into();
    valid.edges[0].to.path = "src/module/service.rs".into();
    let result = dispatch_call_hierarchy_result("hierarchy-normalized-paths", valid).await;
    assert!(result.success, "{result:?}");
}

#[tokio::test]
async fn disconnected_agent_blocks_document_diagnostics_dispatch() {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let project = register_lsp_agent(&runtime, "offline-lsp", "demo", tmp.path(), true).await;
    runtime
        .runner_registry
        .reconcile_disconnect("offline-lsp", "inst")
        .await;
    let result = runtime
        .dispatch_with_auth(
            ToolCall::DocumentDiagnostics {
                project,
                path: "src/main.rs".into(),
                limit: None,
                session_id: None,
            },
            Some(&auth_context(None, true)),
        )
        .await;
    assert!(!result.success, "{result:?}");
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("not connected"),
        "{result:?}"
    );
}

#[tokio::test]
async fn lsp_status_unavailable_still_succeeds() {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let project = register_lsp_agent(&runtime, "lsp-agent", "demo", tmp.path(), true).await;
    let auth = auth_context(None, true);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::LspStatus {
                        project,
                        session_id: None,
                    },
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    complete_lsp_agent_request(
        &runtime,
        "lsp-agent",
        LspStatusResult {
            project: "demo".into(),
            detected_languages: vec![],
            servers: vec![crate::lsp_bridge::LspServerStatusEntry {
                language: "rust".into(),
                server: "rust-analyzer".into(),
                available: false,
                running: false,
                status: LspAvailabilityStatus::Unavailable,
                source: None,
                position_encoding: None,
            }],
            warnings: vec![],
        },
    )
    .await;
    let result = task.await.unwrap();
    assert!(result.success, "{result:?}");
    assert_eq!(result.output["servers"][0]["available"], false);
    assert_eq!(result.output["servers"][0]["status"], "unavailable");
    assert!(!result.output.to_string().contains("file://"));
    let _ = auth;
}

#[tokio::test]
async fn document_symbols_and_locations_are_normalized() {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let project = register_lsp_agent(&runtime, "lsp-nav", "demo", tmp.path(), true).await;

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::DocumentSymbols {
                        project,
                        path: "src/main.rs".into(),
                        limit: Some(10),
                        session_id: None,
                    },
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    complete_lsp_agent_request(
        &runtime,
        "lsp-nav",
        DocumentSymbolsResult {
            project: "demo".into(),
            path: "src/main.rs".into(),
            language: "rust".into(),
            symbols: vec![PublicSymbol {
                name: "main".into(),
                kind: "function".into(),
                kind_code: 12,
                detail: None,
                range: PublicRange {
                    start: PublicPosition { line: 1, column: 1 },
                    end: PublicPosition { line: 1, column: 4 },
                },
                selection_range: PublicRange {
                    start: PublicPosition { line: 1, column: 1 },
                    end: PublicPosition { line: 1, column: 4 },
                },
                children: vec![],
            }],
            total_count: 1,
            returned_count: 1,
            truncated: false,
            external_results_omitted: 0,
            invalid_results_omitted: 0,
        },
    )
    .await;
    let result = task.await.unwrap();
    assert!(result.success, "{result:?}");
    assert_eq!(result.output["symbols"][0]["name"], "main");
    assert_eq!(result.output["path"], "src/main.rs");
    assert!(result.output["project"]
        .as_str()
        .unwrap()
        .starts_with("agent:"));
}

#[tokio::test]
async fn document_diagnostics_dispatches_typed_result_without_process_output() {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let project = register_lsp_agent(&runtime, "lsp-diagnostics", "demo", tmp.path(), true).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::DocumentDiagnostics {
                        project,
                        path: "src/main.rs".into(),
                        limit: Some(100),
                        session_id: None,
                    },
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    complete_lsp_agent_request(
        &runtime,
        "lsp-diagnostics",
        document_diagnostics_result("src/main.rs"),
    )
    .await;
    let result = task.await.unwrap();
    assert!(result.success, "{result:?}");
    assert_eq!(result.output["diagnostics"][0]["severity"], "warning");
    assert_eq!(result.output["status"], "complete");
    assert_eq!(result.output["clean"], false);
    let serialized = result.output.to_string();
    assert!(!serialized.contains("stdout"));
    assert!(!serialized.contains("stderr"));
    assert!(!serialized.contains("file://"));
}

#[tokio::test]
async fn document_diagnostics_timeout_is_not_reported_as_clean_success() {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let project = register_lsp_agent(
        &runtime,
        "lsp-diagnostics-timeout",
        "demo",
        tmp.path(),
        true,
    )
    .await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::DocumentDiagnostics {
                        project,
                        path: "src/main.rs".into(),
                        limit: Some(100),
                        session_id: None,
                    },
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    let mut timeout = document_diagnostics_result("src/main.rs");
    timeout.diagnostics.clear();
    timeout.total_count = 0;
    timeout.returned_count = 0;
    timeout.status = DocumentDiagnosticsStatus::Timeout;
    timeout.clean = None;
    timeout.published_version = None;
    complete_lsp_agent_request(&runtime, "lsp-diagnostics-timeout", timeout).await;

    let result = task.await.unwrap();
    assert!(!result.success, "{result:?}");
    assert_eq!(result.output["status"], "timeout");
    assert_eq!(result.output["clean"], Value::Null);
    assert_eq!(result.output["diagnostics"], json!([]));
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains(error_codes::LSP_REQUEST_TIMEOUT)));
}

#[tokio::test]
async fn document_diagnostics_result_boundary_rejects_embedded_absolute_paths() {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let project =
        register_lsp_agent(&runtime, "lsp-diagnostic-path", "demo", tmp.path(), true).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::DocumentDiagnostics {
                        project,
                        path: "src/main.rs".into(),
                        limit: Some(100),
                        session_id: None,
                    },
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    let mut result = document_diagnostics_result("src/main.rs");
    result.diagnostics[0].message = "compiler opened /tmp/private.rs".into();
    complete_lsp_agent_request(&runtime, "lsp-diagnostic-path", result).await;
    let result = task.await.unwrap();
    assert!(!result.success, "{result:?}");
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("malformed_agent_lsp_result"),
        "{result:?}"
    );
}

#[tokio::test]
async fn hover_dispatches_typed_normalized_result() {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let project = register_lsp_agent(&runtime, "lsp-hover", "demo", tmp.path(), true).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::Hover {
                        project,
                        path: "src/main.rs".into(),
                        line: 1,
                        column: 1,
                        session_id: None,
                    },
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    complete_lsp_agent_request(&runtime, "lsp-hover", hover_result("src/main.rs")).await;
    let result = task.await.unwrap();
    assert!(result.success, "{result:?}");
    assert_eq!(result.output["hover"]["kind"], "markdown");
    assert_eq!(result.output["path"], "src/main.rs");
    assert!(!result.output.to_string().contains("file://"));
}

#[tokio::test]
async fn workspace_symbols_dispatches_typed_bounded_result() {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let project = register_lsp_agent(&runtime, "lsp-workspace", "demo", tmp.path(), true).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::WorkspaceSymbols {
                        project,
                        query: "  ToolRuntime  ".into(),
                        limit: Some(50),
                        session_id: None,
                    },
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    complete_lsp_agent_request(&runtime, "lsp-workspace", workspace_symbols_result()).await;
    let result = task.await.unwrap();
    assert!(result.success, "{result:?}");
    assert_eq!(result.output["query"], "ToolRuntime");
    assert_eq!(
        result.output["symbols"][0]["path"],
        "src/tool_runtime/mod.rs"
    );
    assert!(!result.output.to_string().contains("file://"));
}

#[tokio::test]
async fn hover_and_workspace_symbols_validate_arguments_before_agent_enqueue() {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let project = register_lsp_agent(&runtime, "lsp-invalid", "demo", tmp.path(), true).await;
    for call in [
        ToolCall::Hover {
            project: project.clone(),
            path: "src/main.rs".into(),
            line: 0,
            column: 1,
            session_id: None,
        },
        ToolCall::WorkspaceSymbols {
            project: project.clone(),
            query: "   ".into(),
            limit: None,
            session_id: None,
        },
        ToolCall::WorkspaceSymbols {
            project: project.clone(),
            query: "x".repeat(201),
            limit: None,
            session_id: None,
        },
    ] {
        let result = runtime
            .dispatch_with_auth(call, Some(&auth_context(None, true)))
            .await;
        assert!(!result.success, "{result:?}");
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("invalid_arguments"),
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn goto_definition_multiple_locations() {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let project = register_lsp_agent(&runtime, "lsp-def", "demo", tmp.path(), true).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::GotoDefinition {
                        project,
                        path: "src/main.rs".into(),
                        line: 1,
                        column: 1,
                        limit: Some(20),
                        session_id: None,
                    },
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    complete_lsp_agent_request(
        &runtime,
        "lsp-def",
        LocationsResult {
            project: "demo".into(),
            path: "src/main.rs".into(),
            query_position: PublicPosition { line: 1, column: 1 },
            locations: vec![
                PublicLocation {
                    path: "src/main.rs".into(),
                    range: PublicRange {
                        start: PublicPosition { line: 1, column: 1 },
                        end: PublicPosition { line: 1, column: 4 },
                    },
                    target_range: None,
                },
                PublicLocation {
                    path: "src/lib.rs".into(),
                    range: PublicRange {
                        start: PublicPosition { line: 2, column: 1 },
                        end: PublicPosition { line: 2, column: 3 },
                    },
                    target_range: None,
                },
            ],
            total_results: 2,
            returned_count: 2,
            truncated: false,
            external_results_omitted: 1,
            invalid_results_omitted: 0,
        },
    )
    .await;
    let result = task.await.unwrap();
    assert!(result.success, "{result:?}");
    assert_eq!(result.output["returned_count"], 2);
    assert_eq!(result.output["external_results_omitted"], 1);
    assert!(!result.output.to_string().contains("file://"));
}

#[tokio::test]
async fn lsp_result_boundary_rejects_absolute_paths_and_file_uris() {
    for (index, path) in [
        "/tmp/main.rs",
        r"C:\repo\src\main.rs",
        r"\\server\share\main.rs",
        "file:///tmp/main.rs",
    ]
    .into_iter()
    .enumerate()
    {
        let result =
            dispatch_document_symbols_with_result_path(&format!("lsp-path-reject-{index}"), path)
                .await;
        assert!(!result.success, "path must be rejected: {path:?}");
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("malformed_agent_lsp_result"),
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn lsp_result_boundary_accepts_project_relative_paths() {
    let result =
        dispatch_document_symbols_with_result_path("lsp-path-relative", "src/main.rs").await;
    assert!(result.success, "{result:?}");
    assert_eq!(result.output["path"], "src/main.rs");
}

#[tokio::test]
async fn read_only_session_allows_lsp_tools() {
    let runtime = test_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let project = register_lsp_agent(&runtime, "lsp-ro", "demo", tmp.path(), true).await;
    let auth = auth_context(None, true);
    let start = runtime
        .dispatch_with_auth(
            ToolCall::StartSession {
                project: Some(project.clone()),
                title: Some("lsp-ro".into()),
                mode: crate::tool_runtime::SessionMode::ReadOnly,
                deny_write_tools: true,
                deny_shell_tools: true,
                execution_context: None,
            },
            Some(&auth),
        )
        .await;
    assert!(start.success, "{start:?}");
    let session_id = start.output["session_id"].as_str().unwrap().to_string();

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::DocumentDiagnostics {
                        project,
                        path: "src/main.rs".into(),
                        limit: Some(100),
                        session_id: Some(session_id),
                    },
                    Some(&auth_context(None, true)),
                )
                .await
        }
    });
    complete_lsp_agent_request(
        &runtime,
        "lsp-ro",
        document_diagnostics_result("src/main.rs"),
    )
    .await;
    let result = task.await.unwrap();
    assert!(
        result.success,
        "read_only session must allow lsp tools: {result:?}"
    );
}

#[test]
fn malformed_agent_envelope_is_rejected() {
    assert!(parse_runner_lsp_result_envelope("hello").is_err());
    assert!(
        parse_runner_lsp_result_envelope(r#"{"format":"nope","success":true,"result":{}}"#)
            .is_err()
    );
    let ok = RunnerLspResultEnvelope::ok(json!({"ok": true}));
    let parsed = parse_runner_lsp_result_envelope(&ok.to_stdout_json()).unwrap();
    assert!(parsed.success);
}

#[test]
fn typed_payload_rejects_arbitrary_operation() {
    let bad = r#"{"project_id":"p","request":{"operation":"arbitrary_passthrough","method":"workspace/symbol"}}"#;
    assert!(serde_json::from_str::<RunnerLspPayload>(bad).is_err());
    let old_request = r#"{"request_id":"r","client_id":"c","command":"echo","timeout_secs":1,"requested_by":"t","created_at":1}"#;
    let req: crate::runner_protocol::RunnerRequest = serde_json::from_str(old_request).unwrap();
    assert!(req.lsp.is_none());
}
