use super::*;

#[test]
fn computer_launch_application_output_schema_has_closed_native_platforms() {
    let schema =
        crate::tool_runtime::registry::output_schema_for_tool("computer_launch_application");
    let validate = |value: &Value| {
        crate::tool_runtime::startup_brief::validate_schema_instance_for_test(value, &schema)
    };
    let application_id = "application_0123456789abcdef0123456789abcdef";
    for platform in ["windows", "macos"] {
        let output =
            serde_json::to_value(crate::tool_runtime::tool_result::ToolResult::ok(json!({
                "platform": platform,
                "application_id": application_id,
                "success": true,
            })))
            .unwrap();
        validate(&output).unwrap_or_else(|error| panic!("{platform}: {error}"));
    }

    let stale = serde_json::to_value(
        crate::tool_runtime::tool_result::ToolResult::err_with_output(
            "stale application",
            json!({
                "error_kind": "stale_application",
                "message": "application identity is stale",
                "application_id": application_id,
                "state_changed": false,
                "execution_state": "not_started",
                "recovery_kind": "reobserve",
                "recovery_tool": "computer_list_applications"
            }),
        ),
    )
    .unwrap();
    validate(&stale).unwrap();
    let mut invalid_recovery = stale.clone();
    invalid_recovery["output"]["recovery_kind"] = json!("blind_retry");
    assert!(validate(&invalid_recovery).is_err());
    let mut invalid_tool_class = stale.clone();
    invalid_tool_class["output"]["recovery_kind"] = json!("fix_input");
    assert!(validate(&invalid_tool_class).is_err());

    let mut recovery_on_success =
        serde_json::to_value(crate::tool_runtime::tool_result::ToolResult::ok(json!({
            "platform": "macos",
            "application_id": application_id,
            "success": true,
        })))
        .unwrap();
    recovery_on_success["output"]["recovery_kind"] = json!("none");
    assert!(validate(&recovery_on_success).is_err());

    let unsupported =
        serde_json::to_value(crate::tool_runtime::tool_result::ToolResult::ok(json!({
            "platform": "linux",
            "application_id": application_id,
            "success": true,
        })))
        .unwrap();
    assert!(validate(&unsupported).is_err());

    let extra = serde_json::to_value(crate::tool_runtime::tool_result::ToolResult::ok(json!({
        "platform": "macos",
        "application_id": application_id,
        "success": true,
        "bundle_url": "PRIVATE",
    })))
    .unwrap();
    assert!(validate(&extra).is_err());
}

#[test]
fn read_files_output_schema_rejects_sparse_item_over_default_limit() {
    let schema = crate::tool_runtime::registry::output_schema_for_tool("read_files");
    let default_limit = webcodex_workspace::file_read_range::EffectiveRange::new(None, None).limit;
    let sparse_batch_over_default_limit = json!({
        "success": true,
        "output": {
            "items": [{
                "index": 0,
                "path": "src/lib.rs",
                "success": true,
                "output": {
                    "text": "hello",
                    "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "total_lines": default_limit + 1
                },
                "error": null
            }]
        },
        "error": null
    });
    assert!(
        crate::tool_runtime::startup_brief::validate_schema_instance_for_test(
            &sparse_batch_over_default_limit,
            &schema,
        )
        .is_err(),
        "complete sparse read_files item cannot claim more lines than the default range can return"
    );
}
