use super::*;

#[test]
fn apply_text_edits_metadata_mcp_openapi_consistency() {
    use crate::tool_runtime::tool_definition::TOOL_DISCOVERY_GROUP_EDIT;

    // Known name + spec + metadata coverage. registered_tool_specs() backs
    // both the list_tools runtime tool and MCP tools/list (parity is enforced
    // by mcp_tools_list_parity_with_rest_tools_list), so checking specs covers
    // both surfaces.
    assert!(is_known_tool_name("apply_text_edits"));
    let specs = registered_tool_specs();
    assert!(
        specs.iter().any(|s| s.name == "apply_text_edits"),
        "apply_text_edits must appear in registered tool specs (list_tools + MCP tools/list)"
    );
    for spec in &specs {
        assert!(
            is_known_tool_name(&spec.name),
            "{} must be recognized by ToolCall",
            spec.name
        );
    }
    assert!(
        specs.len()
            == crate::tool_runtime::tool_definition::model_visible_tool_definitions()
                .count(),
        "public specs must cover every model-visible runtime tool (ModelHidden tools are dispatched but have no spec)"
    );
    assert!(crate::tool_runtime::metadata::lookup_tool_metadata("apply_text_edits").is_some());
    // The edit category includes the new tool.
    let cats = registered_tool_categories();
    let edit = cats[TOOL_DISCOVERY_GROUP_EDIT]
        .as_array()
        .expect("edit category present");
    assert!(edit.iter().any(|v| v == "apply_text_edits"));
    // OpenAPI ToolCallRequest description lists the name; operation count
    // stays within the GPT Actions budget because no dedicated operation is added.
    let spec = crate::openapi::build_openapi_spec();
    let tool_desc = &spec["components"]["schemas"]["ToolCallRequest"]["properties"]["tool"]
        ["description"]
        .as_str()
        .unwrap();
    assert!(
        tool_desc.contains("apply_text_edits"),
        "OpenAPI ToolCallRequest.tool should list apply_text_edits"
    );
    let count: usize = spec["paths"]
        .as_object()
        .unwrap()
        .values()
        .map(|m| m.as_object().unwrap().len())
        .sum();
    assert_eq!(count, 16, "OpenAPI operation count must remain 16");
}
