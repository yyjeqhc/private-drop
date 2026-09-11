use super::*;
use webcodex_core::runner_protocol::RAW_SHELL_COMMAND_MAX_BYTES;

macro_rules! assert_schema_fields {
    (
        $properties:expr,
        $context:expr,
        present: [$($present:expr),* $(,)?]
        $(, absent: [$($absent:expr),* $(,)?])?
        $(,)?
    ) => {{
        let properties = $properties;
        let context = $context;
        $(
            assert!(
                properties.contains_key($present),
                "{context}: missing schema field {}",
                $present
            );
        )*
        $(
            $(
                assert!(
                    !properties.contains_key($absent),
                    "{context}: unexpected schema field {}",
                    $absent
                );
            )*
        )?
    }};
}

#[test]
fn tool_specs_names_are_unique() {
    let specs = registered_tool_specs();
    let mut names = specs
        .iter()
        .map(|spec| spec.name.clone())
        .collect::<Vec<_>>();
    names.sort();
    let mut deduped = names.clone();
    deduped.dedup();
    assert_eq!(names, deduped, "tool names must be unique");
}

#[test]
fn tool_specs_names_are_snake_case() {
    for spec in registered_tool_specs() {
        assert!(!spec.name.contains('-'));
        assert!(
            spec.name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "tool '{}' should be snake_case",
            spec.name
        );
    }
}

#[test]
fn tool_specs_derive_contract_fields_from_name() {
    for spec in registered_tool_specs() {
        assert_eq!(
            spec.output_schema,
            output_schema_for_tool(&spec.name),
            "{}",
            spec.name
        );
        assert_eq!(
            spec.annotations,
            tool_annotations(&spec.name),
            "{}",
            spec.name
        );
    }
}

#[test]
fn tool_specs_input_schemas_are_objects() {
    for spec in registered_tool_specs() {
        let schema = &spec.input_schema;
        assert!(schema.is_object(), "{}", spec.name);
        assert_eq!(schema["type"].as_str(), Some("object"), "{}", spec.name);
        assert!(schema["properties"].is_object(), "{}", spec.name);
        assert!(schema["required"].is_array(), "{}", spec.name);
        assert_eq!(schema["additionalProperties"], false, "{}", spec.name);
    }
}

#[test]
fn search_project_texts_query_schema_declares_bounded_advanced_inputs() {
    let specs = registered_tool_specs();
    let search = spec_named(&specs, "search_project_texts");
    let properties = search.input_schema["properties"]["queries"]["items"]["properties"]
        .as_object()
        .unwrap();

    assert_schema_fields!(
        properties,
        "search_project_texts query schema",
        present: ["include_globs", "exclude_globs", "result_mode", "timeout_secs"]
    );
    for field in ["include_globs", "exclude_globs"] {
        assert_eq!(properties[field]["type"], "array");
        assert_eq!(properties[field]["maxItems"], 32);
        assert_eq!(properties[field]["items"]["type"], "string");
        assert_eq!(properties[field]["items"]["minLength"], 1);
        assert_eq!(properties[field]["items"]["maxLength"], 256);
    }
    assert!(properties["timeout_secs"].get("minimum").is_none());
    assert!(properties["timeout_secs"].get("maximum").is_none());
    assert_eq!(properties["timeout_secs"]["type"], "integer");
    assert_eq!(properties["timeout_secs"]["default"], 30);
    assert!(properties["timeout_secs"]["description"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase()
        .contains("clamp"));
    assert_eq!(
        properties["result_mode"]["enum"],
        json!(["matches", "files_with_matches", "count"])
    );
}

#[test]
fn batch_inspection_result_budget_schema_uses_explicit_512_kib_ceiling() {
    let specs = registered_tool_specs();
    for name in ["read_files", "search_project_texts"] {
        let schema = &spec_named(&specs, name).input_schema;
        let budget = &schema["properties"]["max_result_bytes"];
        assert_eq!(budget["minimum"], 8 * 1024, "{name}");
        assert_eq!(budget["default"], 64 * 1024, "{name}");
        assert_eq!(budget["maximum"], 512 * 1024, "{name}");

        let request = |max_result_bytes| match name {
            "read_files" => json!({
                "project": "demo",
                "items": [{"path": "src/lib.rs"}],
                "max_result_bytes": max_result_bytes
            }),
            _ => json!({
                "project": "demo",
                "queries": [{"pattern": "needle"}],
                "max_result_bytes": max_result_bytes
            }),
        };
        assert!(test_support::validate_schema_instance(&request(256 * 1024), schema).is_ok());
        assert!(test_support::validate_schema_instance(&request(512 * 1024), schema).is_ok());
        assert!(test_support::validate_schema_instance(&request(512 * 1024 + 1), schema).is_err());
    }
}

#[test]
fn git_diff_hunks_page_budget_schema_is_producer_scoped_and_bounded() {
    let specs = registered_tool_specs();
    let schema = &spec_named(&specs, "git_diff_hunks").input_schema;
    let page = &schema["properties"]["max_page_bytes"];
    assert_eq!(page["minimum"], 16 * 1024);
    assert_eq!(page["default"], 64 * 1024);
    assert_eq!(page["maximum"], 192 * 1024);
    let description = page["description"].as_str().unwrap().to_ascii_lowercase();
    assert!(description.contains("producer page"));
    assert!(description.contains("final serialized model result"));
    for bytes in [16 * 1024, 64 * 1024, 192 * 1024] {
        assert!(test_support::validate_schema_instance(
            &json!({"project":"demo","max_page_bytes":bytes}),
            schema,
        )
        .is_ok());
    }
    assert!(test_support::validate_schema_instance(
        &json!({"project":"demo","max_page_bytes":192 * 1024 + 1}),
        schema,
    )
    .is_err());
}

#[test]
fn list_project_files_paging_schema_keeps_cardinality_bounded() {
    let specs = registered_tool_specs();
    let schema = &spec_named(&specs, "list_project_files").input_schema;
    let properties = &schema["properties"];
    assert_eq!(properties["limit"]["default"], 200);
    assert!(properties["limit"]["description"]
        .as_str()
        .unwrap()
        .contains("1..500"));
    assert_eq!(properties["offset"]["minimum"], 0);
    assert_eq!(properties["offset"]["default"], 0);
    assert!(properties["offset"]["description"]
        .as_str()
        .unwrap()
        .contains("next_offset"));
    assert!(test_support::validate_schema_instance(
        &json!({"project":"demo","limit":200,"offset":500}),
        schema,
    )
    .is_ok());
}

#[test]
fn sync_validation_and_run_shell_timeout_schema_bounds() {
    let specs = registered_tool_specs();
    for (name, default) in [
        ("cargo_check", 600),
        ("cargo_test", 1800),
        ("go_test", 1800),
    ] {
        let spec = spec_named(&specs, name);
        let timeout = &spec.input_schema["properties"]["timeout_secs"];
        assert_eq!(timeout["type"], "integer", "{name}");
        assert_eq!(timeout["minimum"], 1, "{name}");
        assert_eq!(timeout["maximum"], 3600, "{name}");
        assert_eq!(timeout["default"], default, "{name}");
        let desc = timeout["description"].as_str().unwrap_or("");
        assert!(desc.contains("3600") && desc.to_ascii_lowercase().contains("job"));

        let sync_wait = &spec.input_schema["properties"]["sync_wait_secs"];
        assert_eq!(sync_wait["type"], "integer", "{name}");
        assert_eq!(sync_wait["minimum"], 1, "{name}");
        assert_eq!(sync_wait["maximum"], 60, "{name}");
        assert!(sync_wait.get("default").is_none(), "{name}");
        let desc = sync_wait["description"].as_str().unwrap_or("");
        assert!(desc.contains("same execution"), "{name}: {desc}");
        assert!(
            desc.contains("never extends timeout_secs"),
            "{name}: {desc}"
        );
    }
    let cargo_fmt = spec_named(&specs, "cargo_fmt");
    let timeout = &cargo_fmt.input_schema["properties"]["timeout_secs"];
    assert_eq!(timeout["type"], "integer");
    assert_eq!(timeout["minimum"], 1);
    assert_eq!(timeout["maximum"], 3600);
    assert_eq!(timeout["default"], 120);
    let sync_wait = &cargo_fmt.input_schema["properties"]["sync_wait_secs"];
    assert_eq!(sync_wait["type"], "integer");
    assert_eq!(sync_wait["minimum"], 1);
    assert_eq!(sync_wait["maximum"], 60);
    assert!(sync_wait.get("default").is_none());
    assert_eq!(
        cargo_fmt.input_schema["allOf"][0]["then"]["properties"]["timeout_secs"]["maximum"],
        3600
    );
    assert_eq!(
        cargo_fmt.input_schema["allOf"][0]["else"]["properties"]["timeout_secs"]["maximum"],
        120
    );
    assert_eq!(
        cargo_fmt.input_schema["allOf"][0]["else"]["properties"]["sync_wait_secs"]["type"],
        "null"
    );

    let run_shell = spec_named(&specs, "run_shell");
    let timeout = &run_shell.input_schema["properties"]["timeout_secs"];
    assert_eq!(timeout["type"], "integer");
    assert_eq!(timeout["minimum"], 1);
    assert_eq!(timeout["maximum"], 120);
    assert_eq!(timeout["default"], 60);

    let search = spec_named(&specs, "search_project_texts");
    assert!(
        search.input_schema["properties"]["queries"]["items"]["properties"]["timeout_secs"]
            .get("maximum")
            .is_none()
    );
}

#[test]
fn cargo_test_schema_explains_execution_proof_policy() {
    let specs = registered_tool_specs();
    let spec = spec_named(&specs, "cargo_test");
    let properties = spec.input_schema["properties"].as_object().unwrap();
    let require_tests = properties["require_tests"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(require_tests.contains("Omission"), "{require_tests}");
    assert!(require_tests.contains("non-zero"), "{require_tests}");
    assert!(
        require_tests.contains("false is an explicit opt-out"),
        "{require_tests}"
    );
    assert!(
        require_tests.contains("true requires proof"),
        "{require_tests}"
    );

    let no_run = properties["no_run"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(no_run.contains("compile-only"), "{no_run}");
    assert!(
        no_run.contains("does not require executed-test-count proof"),
        "{no_run}"
    );
    assert!(spec
        .description
        .contains("Normal execution requires non-zero"));
    assert!(spec.description.contains("require_tests=false opts out"));
    assert!(spec.description.contains("no_run=true is compile-only"));
}

#[test]
fn raw_shell_tools_expose_the_shared_authored_command_bound() {
    let specs = registered_tool_specs();
    for name in ["run_shell", "run_job", "session_shell_exec"] {
        let spec = spec_named(&specs, name);
        let command = &spec.input_schema["properties"]["command"];
        assert_eq!(command["maxLength"], RAW_SHELL_COMMAND_MAX_BYTES, "{name}");
        let description = command["description"].as_str().unwrap_or_default();
        assert!(description.contains("16000") || description.contains("16,000"));
    }
}

#[test]
fn run_process_schema_is_small_bounded_and_has_no_shell_or_environment_input() {
    let specs = registered_tool_specs();
    let spec = spec_named(&specs, "run_process");
    let properties = spec.input_schema["properties"].as_object().unwrap();
    assert_eq!(
        spec.input_schema["required"],
        json!(["project", "executable"])
    );
    assert_schema_fields!(
        properties,
        "run_process input schema",
        present: ["project", "executable", "args", "cwd", "stdin", "timeout_secs", "purpose", "session_id"],
        absent: ["shell", "env", "environment"]
    );
    assert_eq!(properties["executable"]["minLength"], 1);
    assert_eq!(properties["executable"]["maxLength"], 1024);
    assert_eq!(properties["args"]["type"], "array");
    assert_eq!(properties["args"]["maxItems"], 256);
    assert_eq!(properties["args"]["items"]["type"], "string");
    assert_eq!(properties["args"]["items"]["maxLength"], 8192);
    assert_eq!(properties["args"]["default"], json!([]));
    assert_eq!(
        properties["stdin"]["anyOf"],
        json!([{"type": "string", "maxLength": 65_536}, {"type": "null"}])
    );
    assert_eq!(properties["cwd"]["maxLength"], 1024);
    assert_eq!(properties["timeout_secs"]["minimum"], 1);
    assert_eq!(properties["timeout_secs"]["maximum"], 3600);
    assert_eq!(properties["timeout_secs"]["default"], 60);
    assert_eq!(spec.input_schema["additionalProperties"], false);
}

#[test]
fn run_script_schema_is_typed_bounded_and_hides_execution_infrastructure() {
    let specs = registered_tool_specs();
    let spec = spec_named(&specs, "run_script");
    let properties = spec.input_schema["properties"].as_object().unwrap();
    assert_eq!(
        spec.input_schema["required"],
        json!(["project", "language", "script"])
    );
    assert_schema_fields!(
        properties,
        "run_script input schema",
        present: ["project", "language", "script", "args", "stdin", "cwd", "timeout_secs", "purpose", "session_id"],
        absent: ["command", "executable", "interpreter", "interpreter_path", "interpreter_args", "shell", "env", "environment", "temp_file", "profile", "pty", "allow_cross_project_session"]
    );
    assert_eq!(
        properties["language"]["enum"],
        json!(["sh", "bash", "powershell", "javascript", "typescript"])
    );
    let language_description = properties["language"]["description"]
        .as_str()
        .expect("run_script language description");
    for phrase in [
        ".mjs ESM",
        ".mts ESM",
        "erasable type stripping",
        "Node.js 22.6.0 or newer",
        "callers cannot provide a runtime path or runtime flags",
    ] {
        assert!(
            language_description.contains(phrase),
            "run_script language description is missing {phrase:?}: {language_description}"
        );
    }
    assert_eq!(properties["script"]["minLength"], 1);
    assert_eq!(properties["script"]["maxLength"], 512 * 1024);
    assert_eq!(properties["args"]["type"], "array");
    assert_eq!(properties["args"]["maxItems"], 256);
    assert_eq!(properties["args"]["items"]["type"], "string");
    assert_eq!(properties["args"]["items"]["maxLength"], 8192);
    assert_eq!(properties["args"]["default"], json!([]));
    assert_eq!(
        properties["stdin"]["anyOf"],
        json!([{"type": "string", "maxLength": 65_536}, {"type": "null"}])
    );
    assert_eq!(properties["cwd"]["maxLength"], 1024);
    assert_eq!(properties["timeout_secs"]["minimum"], 1);
    assert_eq!(properties["timeout_secs"]["maximum"], 3600);
    assert_eq!(properties["timeout_secs"]["default"], 60);
    assert_eq!(spec.input_schema["additionalProperties"], false);
}

#[test]
fn cargo_fmt_conditional_timeout_schema_matches_contract() {
    let specs = registered_tool_specs();
    let schema = &spec_named(&specs, "cargo_fmt").input_schema;
    let validates = |value: &Value| test_support::validate_schema_instance(value, schema).is_ok();

    assert!(validates(
        &json!({"project": "demo", "check": true, "timeout_secs": 3600})
    ));
    assert!(validates(
        &json!({"project": "demo", "check": true, "timeout_secs": 3600, "sync_wait_secs": 1})
    ));
    assert!(validates(
        &json!({"project": "demo", "check": true, "timeout_secs": 3600, "sync_wait_secs": 60})
    ));
    assert!(!validates(
        &json!({"project": "demo", "check": true, "timeout_secs": 3600, "sync_wait_secs": 0})
    ));
    assert!(!validates(
        &json!({"project": "demo", "check": true, "timeout_secs": 3600, "sync_wait_secs": 61})
    ));
    assert!(!validates(
        &json!({"project": "demo", "check": true, "timeout_secs": 3601})
    ));
    assert!(validates(
        &json!({"project": "demo", "check": false, "timeout_secs": 120})
    ));
    assert!(!validates(
        &json!({"project": "demo", "check": false, "timeout_secs": 120, "sync_wait_secs": 1})
    ));
    assert!(!validates(
        &json!({"project": "demo", "timeout_secs": 120, "sync_wait_secs": 1})
    ));
    assert!(!validates(
        &json!({"project": "demo", "check": false, "timeout_secs": 121})
    ));
    assert!(!validates(&json!({"project": "demo", "timeout_secs": 121})));
    assert!(validates(
        &json!({"project": "demo", "check": true, "result_expectation": "failure"})
    ));
    assert!(!validates(
        &json!({"project": "demo", "check": false, "result_expectation": "failure"})
    ));
    assert!(!validates(
        &json!({"project": "demo", "result_expectation": "observe"})
    ));
}

#[test]
fn tool_specs_required_fields_match_declared_properties() {
    for spec in registered_tool_specs() {
        let properties = spec.input_schema["properties"].as_object().unwrap();
        let required = spec.input_schema["required"].as_array().unwrap();
        for field in required {
            let field = field.as_str().unwrap();
            assert!(properties.contains_key(field), "{}: {field}", spec.name);
        }
    }
}

#[test]
fn tool_specs_output_schemas_are_objects() {
    for spec in registered_tool_specs() {
        let schema = &spec.output_schema;
        assert_eq!(schema["type"].as_str(), Some("object"), "{}", spec.name);
        assert!(schema["properties"].is_object(), "{}", spec.name);
        assert!(
            schema["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|value| value == "success")),
            "{} output schema must require success",
            spec.name
        );
    }
}

#[test]
fn tool_specs_optional_fields_are_not_required() {
    let specs = registered_tool_specs();
    let run_shell = spec_named(&specs, "run_shell");
    let required = required_fields(run_shell);
    assert!(required.contains(&"project".to_string()));
    assert!(required.contains(&"command".to_string()));
    assert!(!required.contains(&"timeout_secs".to_string()));
    assert!(!required.contains(&"cwd".to_string()));
    assert_eq!(
        run_shell.input_schema["properties"]["timeout_secs"]["minimum"],
        1
    );
    assert_eq!(
        run_shell.input_schema["properties"]["timeout_secs"]["maximum"],
        120
    );
    assert_eq!(
        run_shell.input_schema["properties"]["timeout_secs"]["default"],
        60
    );

    let read_files = spec_named(&specs, "read_files");
    let required = required_fields(read_files);
    assert!(required.contains(&"project".to_string()));
    assert!(required.contains(&"items".to_string()));
    assert!(!required.contains(&"with_line_numbers".to_string()));

    let search = spec_named(&specs, "search_project_texts");
    let required = required_fields(search);
    assert!(required.contains(&"project".to_string()));
    assert!(required.contains(&"queries".to_string()));
    let query_required = search.input_schema["properties"]["queries"]["items"]["required"]
        .as_array()
        .unwrap();
    assert!(query_required.iter().any(|value| value == "pattern"));
}

#[test]
fn tool_specs_covers_expected_tool_set() {
    let names = registered_tool_names();
    for expected in [
        "list_tools",
        "list_projects",
        "list_runners",
        "runtime_status",
        "create_agent_task",
        "list_agent_tasks",
        "read_agent_task",
        "assign_agent_task",
        "start_agent_task_attempt",
        "start_agent_task_coding_run",
        "reconcile_agent_task_coding_run",
        "heartbeat_agent_task_attempt",
        "complete_agent_task_attempt",
        "run_process",
        "run_script",
        "run_shell",
        "run_job",
        "stop_job",
        "read_files",
        "git_status",
        "git_diff_hunks",
        "git_log",
        "show_changes",
        "workspace_hygiene_check",
        #[cfg(feature = "workspace-checkpoints")]
        "workspace_checkpoint_create",
        #[cfg(feature = "workspace-checkpoints")]
        "workspace_checkpoint_list",
        #[cfg(feature = "workspace-checkpoints")]
        "workspace_checkpoint_show",
        #[cfg(feature = "workspace-checkpoints")]
        "workspace_checkpoint_restore",
        #[cfg(feature = "workspace-checkpoints")]
        "workspace_checkpoint_delete",
        "apply_patch",
        "apply_unified_diff",
        "delete_project_files",
        "git_restore_paths",
        "discard_untracked",
        "project_overview",
        "list_project_tracked_files",
        "list_jobs",
        "write_project_file",
        "save_project_artifact",
        "read_project_artifact_metadata",
        "read_project_artifact",
        "artifact_upload_begin",
        "artifact_upload_chunk",
        "artifact_upload_finish",
        "artifact_upload_abort",
        "register_project",
        "create_project",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "missing {expected}"
        );
    }
}
