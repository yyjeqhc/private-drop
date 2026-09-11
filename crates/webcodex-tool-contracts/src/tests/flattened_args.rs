use super::*;
use std::collections::BTreeSet;
use webcodex_core::workflow_session_contract::TOOL_CALL_RECORDING_SESSION_ID_FIELD;

fn flattened_compatibility_specs() -> Vec<ToolSpec> {
    registered_tool_specs()
}

#[test]
fn accepted_flattened_arg_preferred_order_is_unique_and_declared() {
    use crate::registry::ACCEPTED_FLATTENED_ARG_PREFERRED_ORDER;

    let mut seen = BTreeSet::new();
    for field in ACCEPTED_FLATTENED_ARG_PREFERRED_ORDER {
        assert!(seen.insert(*field), "duplicate preferred field {field}");
    }

    let mut schema_fields = BTreeSet::new();
    for spec in flattened_compatibility_specs() {
        let Some(properties) = spec.input_schema["properties"].as_object() else {
            continue;
        };
        schema_fields.extend(properties.keys().cloned());
    }

    for field in ACCEPTED_FLATTENED_ARG_PREFERRED_ORDER {
        assert!(
            schema_fields.contains(*field),
            "preferred flattened field {field} is not declared by any ToolSpec input schema"
        );
    }
}

#[test]
fn accepted_flattened_args_appends_recorder_field_once() {
    let spec = ToolSpec {
        name: "synthetic_tool".to_string(),
        description: "Synthetic test tool".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "project": {"type": "string"},
                TOOL_CALL_RECORDING_SESSION_ID_FIELD: {"type": "string"}
            },
            "required": [],
            "additionalProperties": false
        }),
        output_schema: json!({"type": "object", "additionalProperties": true}),
        annotations: json!({}),
    };

    let accepted = accepted_flattened_args_for_spec(&spec);
    let recorder_count = accepted
        .iter()
        .filter(|field| field.as_str() == TOOL_CALL_RECORDING_SESSION_ID_FIELD)
        .count();
    assert_eq!(
        recorder_count, 1,
        "accepted_flattened_args must not duplicate recorder metadata: {accepted:?}"
    );
}

#[test]
fn call_runtime_tool_flattened_args_exclude_testing_and_debug_metadata() {
    let mut accepted_fields = BTreeSet::new();
    for spec in flattened_compatibility_specs() {
        accepted_fields.extend(generic_tool_call_flattened_args_for_spec(&spec));
    }

    for field in ["summary_only", "compact"] {
        assert!(
            accepted_fields.contains(field),
            "critical callRuntimeTool flattened arg {field} must remain accepted"
        );
    }
    for field in [
        "expected_failure",
        "expected_failure_kind",
        "assertion_name",
        "test_expect_failure_kind",
        "allow_cross_project_session",
        "include_command_preview",
        "tool",
        "params",
        "arguments",
    ] {
        assert!(
            !accepted_fields.contains(field),
            "runtime flattened compatibility args must not advertise reserved/non-business field {field}"
        );
    }
}
