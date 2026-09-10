use super::*;

fn supports_model_facing_validation_assertion(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "run_process" | "run_script" | "run_shell" | "run_job"
    )
}

fn supports_model_facing_result_expectation(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "run_process"
            | "run_script"
            | "run_shell"
            | "session_shell_exec"
            | "cargo_fmt"
            | "cargo_check"
            | "cargo_test"
            | "go_test"
    )
}

#[test]
fn run_script_tool_call_parser_accepts_declared_languages() {
    for language in ["sh", "bash", "powershell", "javascript"] {
        let parsed = ToolCall::from_tool_name(
            "run_script",
            json!({
                "project": "demo",
                "language": language,
                "script": " ",
                "args": [],
                "stdin": null
            }),
        )
        .unwrap_or_else(|error| panic!("{language} should parse: {error}"));
        assert_eq!(parsed.tool_name(), "run_script");
    }
    assert!(ToolCall::from_tool_name(
        "run_script",
        json!({"project": "demo", "language": "cmd", "script": "echo no"})
    )
    .is_err());
}

#[test]
fn cargo_fmt_tool_call_parser_accepts_contract_timeout() {
    let parsed = ToolCall::from_tool_name(
        "cargo_fmt",
        json!({"project": "demo", "check": true, "timeout_secs": 3600, "sync_wait_secs": 60}),
    )
    .unwrap();
    match parsed {
        ToolCall::CargoFmt {
            check,
            timeout_secs,
            sync_wait_secs,
            ..
        } => {
            assert_eq!(check, Some(true));
            assert_eq!(timeout_secs, Some(3600));
            assert_eq!(sync_wait_secs, Some(60));
        }
        other => panic!("expected cargo_fmt, got {other:?}"),
    }
}

#[test]
fn tool_specs_input_schema_fields_are_declared_and_safe() {
    for spec in registered_tool_specs() {
        let properties = input_schema_properties(&spec);
        let required = spec.input_schema["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{} input schema required array", spec.name));
        let mut seen_required = BTreeSet::new();

        for field in required {
            let field = field
                .as_str()
                .unwrap_or_else(|| panic!("{} required entries must be strings", spec.name));
            assert!(properties.contains_key(field));
            assert!(seen_required.insert(field));
        }
        assert_schema_property_names_are_safe(&spec.name, &spec.input_schema, "input_schema");
    }
}

#[test]
fn tool_specs_expose_only_safe_validation_assertion_metadata() {
    use crate::tool_runtime::sessions::{
        MAX_MODEL_VALIDATION_ASSERTION_NAME_CHARS, TOOL_ACCEPTED_EXIT_CODES_FIELD,
        TOOL_ASSERTION_NAME_FIELD, TOOL_CALL_EXPECTATION_METADATA_FIELDS,
        TOOL_RESULT_EXPECTATION_FIELD,
    };

    for spec in registered_tool_specs() {
        let props = spec.input_schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{} input schema properties", spec.name));
        for &field in TOOL_CALL_EXPECTATION_METADATA_FIELDS {
            let exposed = props.contains_key(field);
            let expected = match field {
                TOOL_ASSERTION_NAME_FIELD => supports_model_facing_validation_assertion(&spec.name),
                TOOL_RESULT_EXPECTATION_FIELD => {
                    supports_model_facing_result_expectation(&spec.name)
                }
                TOOL_ACCEPTED_EXIT_CODES_FIELD => spec.name == "run_process",
                _ => false,
            };
            assert_eq!(exposed, expected, "{} {field}", spec.name);
        }
        if supports_model_facing_validation_assertion(&spec.name) {
            let assertion = &props[TOOL_ASSERTION_NAME_FIELD];
            assert_eq!(assertion["type"], "string", "{}", spec.name);
            assert_eq!(assertion["minLength"], 1, "{}", spec.name);
            assert_eq!(
                assertion["maxLength"], MAX_MODEL_VALIDATION_ASSERTION_NAME_CHARS,
                "{}",
                spec.name
            );
        }
        if supports_model_facing_result_expectation(&spec.name) {
            assert_eq!(
                props[TOOL_RESULT_EXPECTATION_FIELD]["enum"],
                json!(["success", "failure", "observe"]),
                "{}",
                spec.name
            );
        }
        if spec.name == "run_process" {
            let accepted = &props[TOOL_ACCEPTED_EXIT_CODES_FIELD];
            assert_eq!(accepted["type"], "array");
            assert_eq!(accepted["minItems"], 1);
            assert_eq!(accepted["maxItems"], 32);
            assert_eq!(accepted["items"]["type"], "integer");
        }
    }
}

#[test]
fn tool_specs_required_fields_match_deserialization() {
    for spec in registered_tool_specs() {
        let required: Vec<String> = spec.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect();
        let args = sample_tool_args_for_spec(&spec);
        ToolCall::from_tool_name(&spec.name, args.clone())
            .unwrap_or_else(|error| panic!("tool '{}' minimal args failed: {error}", spec.name));

        for field in &required {
            let mut partial = args.as_object().cloned().unwrap_or_default();
            partial.remove(field);
            let err = ToolCall::from_tool_name(&spec.name, Value::Object(partial))
                .err()
                .unwrap_or_else(|| {
                    panic!(
                        "tool '{}' should reject missing required field '{field}'",
                        spec.name
                    )
                });
            assert!(err.contains(field), "{} {field}: {err}", spec.name);
        }
    }
}

#[test]
fn tool_specs_descriptions_fit_model_budget() {
    for spec in registered_tool_specs() {
        assert!(
            spec.description.chars().count()
                <= crate::tool_runtime::MODEL_TOOL_DESCRIPTION_MAX_CHARS,
            "{} description is too long: {} chars (hard budget {})",
            spec.name,
            spec.description.chars().count(),
            crate::tool_runtime::MODEL_TOOL_DESCRIPTION_MAX_CHARS
        );
    }
}

const SENSITIVE_INPUT_FIELD_NAMES: &[&str] = &[
    "token",
    "secret",
    "env",
    "environment",
    "credential",
    "password",
];

fn input_schema_properties(spec: &ToolSpec) -> &serde_json::Map<String, Value> {
    spec.input_schema["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("{} input schema properties object", spec.name))
}

fn assert_schema_property_names_are_safe(tool_name: &str, schema: &Value, path: &str) {
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (field, property_schema) in properties {
            assert!(
                !field.is_empty(),
                "{tool_name} {path} property names must be non-empty"
            );
            let provider_local_tool_field = tool_name == "plugin_tool"
                && path == "input_schema"
                && field == TOOL_CALL_TOOL_FIELD;
            assert!(
                field != TOOL_CALL_TOOL_FIELD || provider_local_tool_field,
                "{tool_name} {path}.{field}"
            );
            let lower = field.to_ascii_lowercase();
            assert!(
                !SENSITIVE_INPUT_FIELD_NAMES.contains(&lower.as_str()),
                "{tool_name} {path}.{field}"
            );
            assert!(!field.starts_with("test_"), "{tool_name} {path}.{field}");
            let expectation_field_allowed = match field.as_str() {
                crate::tool_runtime::sessions::TOOL_ASSERTION_NAME_FIELD => {
                    path == "input_schema" && supports_model_facing_validation_assertion(tool_name)
                }
                crate::tool_runtime::sessions::TOOL_RESULT_EXPECTATION_FIELD => {
                    path == "input_schema" && supports_model_facing_result_expectation(tool_name)
                }
                crate::tool_runtime::sessions::TOOL_ACCEPTED_EXIT_CODES_FIELD => {
                    path == "input_schema" && tool_name == "run_process"
                }
                _ => !crate::tool_runtime::sessions::TOOL_CALL_EXPECTATION_METADATA_FIELDS
                    .contains(&field.as_str()),
            };
            assert!(expectation_field_allowed, "{tool_name} {path}.{field}");
            let nested_path = format!("{path}.properties.{field}");
            assert_schema_property_names_are_safe(tool_name, property_schema, &nested_path);
        }
    }

    if let Some(items) = schema.get("items") {
        let nested_path = format!("{path}.items");
        assert_schema_property_names_are_safe(tool_name, items, &nested_path);
    }

    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(schemas) = schema.get(key).and_then(Value::as_array) {
            for (idx, nested) in schemas.iter().enumerate() {
                let nested_path = format!("{path}.{key}[{idx}]");
                assert_schema_property_names_are_safe(tool_name, nested, &nested_path);
            }
        }
    }
}
