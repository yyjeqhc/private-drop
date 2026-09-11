use super::*;

fn structured_execution_output(
    execution_source: &str,
    execution_state: &str,
    command_started: bool,
    command_completed: bool,
    promoted_to_job: bool,
    terminal: bool,
    job_id: Option<&str>,
    job_status: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "success": promoted_to_job,
        "output": {
            "execution_source": execution_source,
            "execution_state": execution_state,
            "command_started": command_started,
            "command_completed": command_completed,
            "promoted_to_job": promoted_to_job,
            "terminal": terminal,
            "job_id": job_id,
            "job_status": job_status,
            "observation_token": if promoted_to_job { Some("observation") } else { None },
            "activity": if promoted_to_job {
                Some(serde_json::json!({
                    "state": "working",
                    "phase": "process_running",
                    "source": "runner_execution"
                }))
            } else {
                None
            },
            "effective_timeout_secs": 60,
            "sync_wait_secs": 10,
            "async_handoff_available": true
        },
        "error": null
    })
}

fn continuation_feedback_subschema(specs: &[ToolSpec], tool: &str) -> Value {
    let spec = spec_named(specs, tool);
    spec.output_schema["properties"]["output"]["properties"]["continuation_feedback"].clone()
}

fn assert_all_objects_strict(schema: &Value, path: &str, open_boundaries: &[&str]) {
    match schema {
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("object") {
                let additional_properties = object.get("additionalProperties");
                if open_boundaries.contains(&path) {
                    assert!(
                        additional_properties
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        "{path}: documented open boundary should be additionalProperties:true"
                    );
                } else {
                    assert_eq!(
                        additional_properties.and_then(Value::as_bool),
                        Some(false),
                        "{path}: core object must be additionalProperties:false"
                    );
                }
            }
            for (key, child) in object {
                assert_all_objects_strict(child, &format!("{path}.{key}"), open_boundaries);
            }
        }
        Value::Array(array) => {
            for (index, child) in array.iter().enumerate() {
                assert_all_objects_strict(child, &format!("{path}[{index}]"), open_boundaries);
            }
        }
        _ => {}
    }
}

#[test]
fn git_diff_hunks_output_schema_keeps_page_and_model_budgets_distinct() {
    let specs = registered_tool_specs();
    let spec = spec_named(&specs, "git_diff_hunks");
    let output = &spec.output_schema["properties"]["output"]["properties"];
    assert!(output["max_page_bytes"]["description"]
        .as_str()
        .unwrap()
        .contains("producer-page"));
    let recovery = &output["recovery"]["properties"]["continuation"]["properties"]["next_call"]
        ["anyOf"][0]["properties"]["arguments"];
    assert_eq!(
        recovery["properties"]["max_page_bytes"]["minimum"],
        16 * 1024
    );
    assert_eq!(
        recovery["properties"]["max_page_bytes"]["maximum"],
        192 * 1024
    );
    assert!(recovery["required"]
        .as_array()
        .unwrap()
        .iter()
        .any(|field| field == "max_page_bytes"));
}

#[test]
fn git_log_and_directory_listing_expose_parser_ready_next_pages() {
    let specs = registered_tool_specs();

    let git_log =
        &spec_named(&specs, "git_log").output_schema["properties"]["output"]["properties"];
    assert!(git_log["next_skip"]["anyOf"].is_array());
    assert!(git_log["next_skip"]["description"]
        .as_str()
        .unwrap()
        .contains("Exact skip value"));

    let files = &spec_named(&specs, "list_project_files").output_schema["properties"]["output"]
        ["properties"];
    for field in [
        "returned",
        "total_entries",
        "offset",
        "next_offset",
        "truncated",
    ] {
        assert!(
            files.get(field).is_some(),
            "missing list_project_files.{field}"
        );
    }
    assert!(files["next_offset"]["anyOf"].is_array());
    assert!(files["total_entries"]["description"]
        .as_str()
        .unwrap()
        .contains("fully acquired"));
}

#[test]
fn tracked_listing_schema_separates_source_incomplete_from_safe_page_truncation() {
    let specs = registered_tool_specs();
    let properties = output_schema_properties(&specs, "list_project_tracked_files");
    let truncated = properties["truncated"]["description"]
        .as_str()
        .unwrap()
        .to_ascii_lowercase();
    let next_offset = properties["next_offset"]["description"]
        .as_str()
        .unwrap()
        .to_ascii_lowercase();
    let list_truncated = properties["list_truncated"]["description"]
        .as_str()
        .unwrap()
        .to_ascii_lowercase();
    assert!(truncated.contains("completely acquired source"));
    assert!(truncated.contains("list_truncated=true"));
    assert!(next_offset.contains("null"));
    assert!(next_offset.contains("list_truncated=true"));
    assert!(list_truncated.contains("source acquisition"));
    assert!(list_truncated.contains("narrow path"));
}

#[test]
fn continuation_feedback_output_schemas_are_synchronized() {
    let specs = registered_tool_specs();
    for name in ["finish_coding_task", "session_handoff_summary"] {
        let spec = spec_named(&specs, name);
        let properties = &spec.output_schema["properties"]["output"]["properties"];
        assert!(
            properties
                .as_object()
                .is_some_and(|properties| properties.contains_key("continuation_feedback")),
            "{name} output schema should expose continuation_feedback"
        );
    }

    let validation_spec = spec_named(&specs, "validation_summary");
    let validation_properties =
        &validation_spec.output_schema["properties"]["output"]["properties"];
    assert!(
        validation_properties
            .as_object()
            .is_some_and(|properties| properties.contains_key("validation_delta")),
        "validation_summary output schema should expose validation_delta"
    );
}

#[test]
fn continuation_feedback_schema_is_strict_on_core_objects() {
    let specs = registered_tool_specs();
    let schema = continuation_feedback_subschema(&specs, "finish_coding_task");
    assert_all_objects_strict(&schema, "continuation_feedback", &[]);

    let validation_spec = spec_named(&specs, "validation_summary");
    let delta =
        &validation_spec.output_schema["properties"]["output"]["properties"]["validation_delta"];
    assert_eq!(
        delta["additionalProperties"].as_bool(),
        Some(false),
        "validation_delta root must be strict"
    );
    assert_all_objects_strict(delta, "validation_delta", &[]);
}

#[test]
fn continuation_feedback_schema_enums_and_signed_ints_are_stable() {
    let specs = registered_tool_specs();
    let schema = continuation_feedback_subschema(&specs, "finish_coding_task");

    let status_enum = schema["properties"]["status"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(status_enum, ["available", "not_applicable", "unknown"]);

    let validation_status_enum = schema["properties"]["attempt"]["properties"]["validation"]
        ["properties"]["latest_status"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        validation_status_enum,
        [
            "passed",
            "failed",
            "inconclusive",
            "not_run",
            "unknown",
            "unavailable"
        ]
    );

    let boundary_source_enum = schema["properties"]["attempt"]["properties"]["boundary"]
        ["properties"]["source"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        boundary_source_enum,
        [
            "task_instruction",
            "session_start",
            "unavailable",
            "no_events"
        ]
    );

    let outcome_enum = schema["properties"]["attempt"]["properties"]["outcome"]["properties"]
        ["status"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(outcome_enum, ["in_progress", "blocked", "clean", "unknown"]);

    let recovery_state_enum = schema["properties"]["attempt"]["properties"]["jobs"]["properties"]
        ["recovery_state"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        recovery_state_enum,
        [
            "none",
            "recovering",
            "terminal_pending",
            "active",
            "unknown"
        ]
    );

    let comparison_reason_enum = schema["properties"]["validation_delta"]["properties"]
        ["comparison"]["properties"]["reason_code"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        comparison_reason_enum,
        [
            "no_previous_validation",
            "validation_scope_changed",
            "previous_evidence_incomplete",
            "current_evidence_incomplete",
            "parser_changed",
            "parser_identity_unavailable",
            "test_identity_unavailable",
            "insufficient_scope_identity",
            "validation_not_requested"
        ]
    );

    for key in [
        "passed_delta",
        "failed_delta",
        "ignored_delta",
        "total_delta",
    ] {
        assert_eq!(
            schema["properties"]["validation_delta"]["properties"]["counts"]["properties"][key]
                ["type"],
            "integer",
            "{key} must be a signed integer"
        );
    }
}

#[test]
fn observe_jobs_failure_item_schema_closes_recovery_metadata() {
    let schema = output_schema_for_tool("observe_jobs");
    let validate = |value: &Value| test_support::validate_schema_instance(value, &schema);
    let result = json!({
        "success": true,
        "output": {
            "requested_count": 1,
            "returned_count": 1,
            "succeeded_count": 0,
            "failed_count": 1,
            "items": [{
                "index": 0,
                "job_id": "job-missing",
                "success": false,
                "output": null,
                "error_kind": "unknown_job",
                "recovery_kind": "reobserve",
                "recovery_tool": "list_jobs",
                "error": "unknown job"
            }],
            "wait": {
                "outcome": "item_error",
                "waited_ms": 0
            },
            "changed_count": 0,
            "terminal_count": 0,
            "output_truncated": false,
            "next_index": null
        },
        "error": null
    });
    validate(&result).unwrap();

    let mut invalid_kind = result.clone();
    invalid_kind["output"]["items"][0]["recovery_kind"] = json!("blind_retry");
    assert!(validate(&invalid_kind).is_err());

    let mut invalid_tool = result;
    invalid_tool["output"]["items"][0]["recovery_tool"] = json!("computer_list_windows");
    assert!(validate(&invalid_tool).is_err());
}

#[test]
fn read_continuation_output_schemas_accept_actionable_recovery_shapes() {
    let read_file = output_schema_for_tool("read_file");
    test_support::validate_schema_instance(
        &json!({
            "success": true,
            "output": {
                "text": "two",
                "format": "plain",
                "path": "src/lib.rs",
                "sha256": "a".repeat(64),
                "start_line": 2,
                "limit": 1,
                "total_lines": 3,
                "returned_lines": 1,
                "end_line": 2,
                "has_more": true,
                "next_start_line": 3,
                "continuation": {
                    "kind": "read_range",
                    "safe_cursor": true,
                    "source_sha256": "a".repeat(64),
                    "snapshot_stable": false,
                    "suggested_call": {
                        "tool": "read_file",
                        "arguments": {
                            "project": "agent:oe:demo",
                            "path": "src/lib.rs",
                            "session_id": "wc_sess_demo",
                            "start_line": 3,
                            "limit": 1
                        }
                    }
                }
            },
            "error": null
        }),
        &read_file,
    )
    .unwrap();

    let read_files = output_schema_for_tool("read_files");
    test_support::validate_schema_instance(
        &json!({
            "success": true,
            "output": {
                "project": "agent:oe:demo",
                "requested_count": 3,
                "returned_count": 1,
                "succeeded_count": 1,
                "failed_count": 0,
                "items": [{
                    "index": 0,
                    "path": "src/0.rs",
                    "success": true,
                    "output": {
                        "text": "first",
                        "format": "plain",
                        "path": "src/0.rs",
                        "sha256": "b".repeat(64),
                        "start_line": 1,
                        "limit": 100,
                        "total_lines": 200,
                        "returned_lines": 50,
                        "end_line": 50,
                        "has_more": true,
                        "next_start_line": 51,
                        "budget_truncated": true,
                        "budget_next_limit": 50
                    },
                    "error": null,
                    "continuation": {
                        "kind": "read_range",
                        "safe_cursor": true,
                        "source_sha256": "b".repeat(64),
                        "snapshot_stable": false,
                        "suggested_call": {
                            "tool": "read_file",
                            "arguments": {
                                "project": "agent:oe:demo",
                                "path": "src/0.rs",
                                "start_line": 51,
                                "limit": 50
                            }
                        }
                    }
                }],
                "output_truncated": true,
                "next_index": 0,
                "truncation_reason": "batch_response_budget",
                "continuation": {
                    "kind": "batch_items",
                    "safe_cursor": true,
                    "next_index": 1,
                    "recommended_order": "after_partial_item",
                    "suggested_call": {
                        "tool": "read_files",
                        "arguments": {
                            "project": "agent:oe:demo",
                            "session_id": "wc_sess_demo",
                            "items": [
                                {"path": "src/1.rs"},
                                {"path": "src/2.rs", "start_line": 4, "limit": 20}
                            ]
                        }
                    }
                }
            },
            "error": null
        }),
        &read_files,
    )
    .unwrap();

    test_support::validate_schema_instance(
        &json!({
            "success": true,
            "output": {
                "project": "agent:oe:demo",
                "requested_count": 1,
                "returned_count": 0,
                "succeeded_count": 0,
                "failed_count": 0,
                "items": [],
                "output_truncated": true,
                "next_index": 0,
                "truncation_reason": "batch_response_budget",
                "continuation": {
                    "kind": "increase_result_budget",
                    "safe_cursor": false,
                    "next_index": 0,
                    "suggested_max_result_bytes": 524288,
                    "suggested_call": {
                        "tool": "read_files",
                        "arguments": {
                            "project": "agent:oe:demo",
                            "items": [{"path": "src/0.rs"}],
                            "max_result_bytes": 524288
                        }
                    }
                }
            },
            "error": null
        }),
        &read_files,
    )
    .unwrap();
}

#[test]
fn read_recovery_schemas_keep_transport_and_recorder_identifiers_private() {
    for tool in ["read_file", "read_files"] {
        let schema = output_schema_for_tool(tool);
        let serialized = serde_json::to_string(&schema).unwrap();
        for forbidden in ["window_id", "client_window", "recording_session_id"] {
            assert!(
                !serialized.contains(forbidden),
                "{tool} recovery schema must not publish {forbidden}: {serialized}"
            );
        }
    }
}

#[test]
fn model_visible_tool_definitions_have_explicit_output_schema_coverage() {
    let specs = registered_tool_specs();
    let default_fields = default_output_schema_field_names();
    let default_schema_names = specs
        .iter()
        .filter(|spec| output_schema_field_names(spec) == default_fields)
        .map(|spec| spec.name.as_str())
        .collect::<Vec<_>>();

    assert!(
        default_schema_names.is_empty(),
        "model-visible tools must declare explicit output schemas: {default_schema_names:?}"
    );
}

#[test]
fn ssh_resource_declares_explicit_canonical_action_output_fields() {
    let specs = registered_tool_specs();
    let spec = spec_named(&specs, "ssh_resource");
    let fields = output_schema_field_names(spec);
    for field in [
        "runner",
        "binding",
        "resources",
        "resource",
        "persisted",
        "active",
        "restart_required",
        "error_kind",
        "dispatch_state",
    ] {
        assert!(fields.contains(field), "ssh_resource missing {field}");
    }
    assert_ne!(fields, default_output_schema_field_names());
}

#[test]
fn key_tool_output_schemas_include_expected_fields() {
    let specs = registered_tool_specs();
    let has_output_field = |name: &str, field: &str| {
        let spec = spec_named(&specs, name);
        spec.output_schema["properties"]["output"]["properties"]
            .as_object()
            .is_some_and(|props| props.contains_key(field))
    };

    for field in [
        "session_id",
        "project",
        "title",
        "execution_context",
        "previous_execution_context",
        "changed",
        "created_at",
        "updated_at",
    ] {
        assert!(
            has_output_field("update_session_context", field),
            "update_session_context missing {field}"
        );
    }

    for field in [
        "duration_ms",
        "exit_code",
        "stdout_tail",
        "stderr_tail",
        "stdout_lines",
        "stderr_lines",
        "stdout_truncated",
        "stderr_truncated",
        "command_started",
        "command_completed",
        "command_ok",
        "failure_kind",
        "tool_failure",
        "purpose",
        "process_summary",
        "cwd",
        "executor",
        "execution_source",
        "execution_state",
        "promoted_to_job",
        "terminal",
        "job_id",
        "job_status",
        "observation_token",
        "effective_timeout_secs",
        "sync_wait_secs",
        "async_handoff_available",
    ] {
        assert!(
            has_output_field("run_process", field),
            "run_process missing {field}"
        );
    }
    assert_eq!(
        output_schema_property(&specs, "run_process", "execution_state")["enum"],
        serde_json::json!([
            "not_started",
            "outcome_unknown",
            "completed",
            "timed_out",
            "queued",
            "running"
        ])
    );
    let run_process_schema = &spec_named(&specs, "run_process").output_schema;
    for (state, command_started, command_completed) in [
        ("not_started", false, false),
        ("outcome_unknown", true, false),
        ("completed", true, true),
        ("timed_out", true, false),
    ] {
        test_support::validate_schema_instance(
            &serde_json::json!({
                "success": false,
                "output": {
                    "execution_state": state,
                    "command_started": command_started,
                    "command_completed": command_completed
                },
                "error": null
            }),
            run_process_schema,
        )
        .unwrap_or_else(|error| panic!("run_process state {state} should validate: {error}"));
    }
    for (state, command_started, job_status) in [
        ("queued", false, "agent_queued"),
        ("running", true, "running"),
    ] {
        test_support::validate_schema_instance(
            &structured_execution_output(
                "run_process",
                state,
                command_started,
                false,
                true,
                false,
                Some("job-1"),
                Some(job_status),
            ),
            run_process_schema,
        )
        .unwrap_or_else(|error| {
            panic!("run_process handoff state {state} should validate: {error}")
        });
    }
    assert!(
        test_support::validate_schema_instance(
            &serde_json::json!({
                "success": false,
                "output": {
                    "execution_state": "started",
                    "command_started": true,
                    "command_completed": false
                },
                "error": null
            }),
            run_process_schema,
        )
        .is_err(),
        "run_process must reject lifecycle states outside the terminal and handoff contract"
    );
    assert!(
        test_support::validate_schema_instance(
            &serde_json::json!({
                "success": false,
                "output": {
                    "execution_state": "not_started",
                    "command_started": true,
                    "command_completed": false
                },
                "error": null
            }),
            run_process_schema,
        )
        .is_err(),
        "run_process must reject lifecycle booleans that contradict execution_state"
    );
    assert!(
        test_support::validate_schema_instance(
            &serde_json::json!({
                "success": false,
                "output": {
                    "failure_kind": "permission_denied",
                    "tool_failure": true
                },
                "error": "permission denied"
            }),
            run_process_schema,
        )
        .is_err(),
        "an execution-style run_process denial must include the canonical lifecycle tuple"
    );
    for (tool_name, schema) in [
        ("run_process", run_process_schema),
        (
            "run_script",
            &spec_named(&specs, "run_script").output_schema,
        ),
    ] {
        assert!(
            test_support::validate_schema_instance(
                &serde_json::json!({
                    "success": true,
                    "output": {
                        "execution_state": "completed",
                        "command_started": true,
                        "command_completed": true,
                        "command_ok": true,
                        "exit_code": 0,
                        "observation_token": "orphan-observation"
                    },
                    "error": null
                }),
                schema,
            )
            .is_err(),
            "{tool_name} must reject an observation_token without the full continuation tuple"
        );
    }
    let process_started_description =
        output_schema_property(&specs, "run_process", "command_started")["description"]
            .as_str()
            .expect("run_process command_started description");
    assert!(process_started_description.contains("outcome_unknown"));

    for field in [
        "duration_ms",
        "exit_code",
        "stdout_tail",
        "stderr_tail",
        "stdout_lines",
        "stderr_lines",
        "stdout_truncated",
        "stderr_truncated",
        "command_started",
        "command_completed",
        "command_ok",
        "failure_kind",
        "tool_failure",
        "purpose",
        "script_summary",
        "language",
        "cwd",
        "executor",
        "execution_source",
        "execution_state",
        "promoted_to_job",
        "terminal",
        "job_id",
        "job_status",
        "observation_token",
        "effective_timeout_secs",
        "sync_wait_secs",
        "async_handoff_available",
    ] {
        assert!(
            has_output_field("run_script", field),
            "run_script missing {field}"
        );
    }
    assert_eq!(
        output_schema_property(&specs, "run_script", "language")["enum"],
        serde_json::json!(["sh", "bash", "powershell", "javascript", "typescript"])
    );
    assert_eq!(
        output_schema_property(&specs, "run_script", "execution_source")["const"],
        "run_script"
    );
    let run_script_schema = &spec_named(&specs, "run_script").output_schema;
    for (state, command_started, command_completed) in [
        ("not_started", false, false),
        ("outcome_unknown", true, false),
        ("completed", true, true),
        ("timed_out", true, false),
    ] {
        test_support::validate_schema_instance(
            &serde_json::json!({
                "success": false,
                "output": {
                    "execution_source": "run_script",
                    "execution_state": state,
                    "command_started": command_started,
                    "command_completed": command_completed
                },
                "error": null
            }),
            run_script_schema,
        )
        .unwrap_or_else(|error| panic!("run_script state {state} should validate: {error}"));
    }
    for (state, command_started, job_status) in [
        ("queued", false, "agent_queued"),
        ("running", true, "running"),
    ] {
        test_support::validate_schema_instance(
            &structured_execution_output(
                "run_script",
                state,
                command_started,
                false,
                true,
                false,
                Some("job-1"),
                Some(job_status),
            ),
            run_script_schema,
        )
        .unwrap_or_else(|error| {
            panic!("run_script handoff state {state} should validate: {error}")
        });
    }
    assert!(
        test_support::validate_schema_instance(
            &serde_json::json!({
                "success": false,
                "output": {
                    "execution_source": "run_script",
                    "execution_state": "completed",
                    "command_started": true,
                    "command_completed": false
                },
                "error": null
            }),
            run_script_schema,
        )
        .is_err(),
        "run_script must reject lifecycle booleans that contradict execution_state"
    );
    assert!(
        test_support::validate_schema_instance(
            &serde_json::json!({
                "success": false,
                "output": {
                    "failure_kind": "permission_denied",
                    "tool_failure": true
                },
                "error": "permission denied"
            }),
            run_script_schema,
        )
        .is_err(),
        "an execution-style run_script denial must include the canonical lifecycle tuple"
    );

    for (tool, execution_source, schema) in [
        ("run_process", "run_process", run_process_schema),
        ("run_script", "run_script", run_script_schema),
    ] {
        let mut promoted_without_job_id = structured_execution_output(
            execution_source,
            "running",
            true,
            false,
            true,
            false,
            Some("job-1"),
            Some("running"),
        );
        promoted_without_job_id["output"]
            .as_object_mut()
            .expect("output object")
            .remove("job_id");
        let impossible = [
            ("promoted execution without job_id", promoted_without_job_id),
            (
                "running execution with command_started=false",
                structured_execution_output(
                    execution_source,
                    "running",
                    false,
                    false,
                    true,
                    false,
                    Some("job-1"),
                    Some("running"),
                ),
            ),
            ("promoted execution with async_handoff_available=false", {
                let mut instance = structured_execution_output(
                    execution_source,
                    "running",
                    true,
                    false,
                    true,
                    false,
                    Some("job-1"),
                    Some("running"),
                );
                instance["output"]["async_handoff_available"] = serde_json::json!(false);
                instance
            }),
            (
                "queued execution with command_completed=true",
                structured_execution_output(
                    execution_source,
                    "queued",
                    false,
                    true,
                    true,
                    false,
                    Some("job-1"),
                    Some("agent_queued"),
                ),
            ),
            (
                "completed execution promoted to a Job",
                structured_execution_output(
                    execution_source,
                    "completed",
                    true,
                    true,
                    true,
                    false,
                    Some("job-1"),
                    Some("completed"),
                ),
            ),
            (
                "not_started execution with command_started=true",
                structured_execution_output(
                    execution_source,
                    "not_started",
                    true,
                    false,
                    false,
                    true,
                    None,
                    None,
                ),
            ),
            (
                "timed_out execution with command_completed=true",
                structured_execution_output(
                    execution_source,
                    "timed_out",
                    true,
                    true,
                    false,
                    true,
                    None,
                    None,
                ),
            ),
        ];
        for (description, instance) in impossible {
            assert!(
                test_support::validate_schema_instance(&instance, schema,).is_err(),
                "{tool} schema must reject {description}"
            );
        }
    }

    for field in [
        "duration_ms",
        "exit_code",
        "stdout_tail",
        "stderr_tail",
        "stdout_lines",
        "stderr_lines",
        "stdout_truncated",
        "stderr_truncated",
        "command_started",
        "command_completed",
        "command_ok",
        "failure_kind",
        "tool_failure",
        "purpose",
        "command_summary",
        "cwd",
        "shell",
        "executor",
        "ssh_resource",
        "execution_state",
    ] {
        assert!(
            has_output_field("run_shell", field),
            "run_shell missing {field}"
        );
    }
    let run_shell_started_description =
        output_schema_property(&specs, "run_shell", "command_started")["description"]
            .as_str()
            .expect("run_shell command_started description");
    assert!(
        run_shell_started_description.contains("outcome_unknown"),
        "run_shell command_started must describe conservative unknown-outcome semantics: {run_shell_started_description}"
    );
    let run_shell_state_description =
        output_schema_property(&specs, "run_shell", "execution_state")["description"]
            .as_str()
            .expect("run_shell execution_state description");
    for state in ["not_started", "outcome_unknown", "completed", "timed_out"] {
        assert!(
            run_shell_state_description.contains(state),
            "run_shell execution_state description missing {state}: {run_shell_state_description}"
        );
    }
    for name in ["cargo_fmt", "cargo_check", "cargo_test", "go_test"] {
        assert!(
            has_output_field(name, "observation_token"),
            "{name} missing promoted Job observation_token"
        );
        assert_eq!(
            output_schema_property(&specs, name, "observation_token")["maxLength"],
            webcodex_core::job_observation::MAX_JOB_OBSERVATION_TOKEN_LEN
        );
        assert!(
            has_output_field(name, "failure_kind"),
            "{name} missing failure_kind"
        );
        let description = output_schema_property(&specs, name, "failure_kind")["description"]
            .as_str()
            .expect("cargo failure_kind description");
        assert!(
            description.contains("validation_failed"),
            "{name} failure_kind description should mention validation_failed: {description}"
        );
        assert!(
            description.contains("outcome_unknown"),
            "{name} failure_kind description should mention outcome_unknown: {description}"
        );
        let state_description = output_schema_property(&specs, name, "execution_state")
            ["description"]
            .as_str()
            .expect("cargo execution_state description");
        for state in [
            "not_started",
            "outcome_unknown",
            "completed",
            "timed_out",
            "queued",
            "running",
        ] {
            assert!(
                state_description.contains(state),
                "{name} execution_state description missing {state}: {state_description}"
            );
        }
    }
    for field in ["tests_detected", "tests_run_count", "zero_tests_run"] {
        for name in ["cargo_test", "go_test"] {
            assert!(has_output_field(name, field), "{name} missing {field}");
        }
        assert!(
            !has_output_field("cargo_fmt", field),
            "cargo_fmt should not expose cargo_test zero-tests metadata field {field}"
        );
        assert!(
            !has_output_field("cargo_check", field),
            "cargo_check should not expose cargo_test zero-tests metadata field {field}"
        );
    }
    assert!(has_output_field("cargo_test", "diagnostics"));
    assert!(has_output_field("cargo_check", "diagnostics"));
    assert!(!has_output_field("cargo_fmt", "diagnostics"));
    let diagnostics_schema = output_schema_property(&specs, "cargo_test", "diagnostics");
    assert_eq!(diagnostics_schema["type"], "object");
    let diagnostics_props = diagnostics_schema["properties"]
        .as_object()
        .expect("cargo_test diagnostics schema properties");
    for field in [
        "available",
        "parser",
        "reason",
        "diagnostic_count",
        "diagnostics",
        "returned_diagnostic_count",
        "diagnostics_truncated",
        "invalid_diagnostics_omitted",
        "test_summary",
        "failed_test_details",
        "failed_test_details_truncated",
        "truncated",
    ] {
        assert!(
            diagnostics_props.contains_key(field),
            "cargo_test diagnostics schema missing {field}"
        );
    }
    for removed in [
        "first_diagnostic",
        "failed_tests",
        "first_failed_test",
        "failed_tests_truncated",
    ] {
        assert!(
            !diagnostics_props.contains_key(removed),
            "cargo_test diagnostics schema must not retain removed field {removed}"
        );
    }
    assert_eq!(diagnostics_props["diagnostics"]["maxItems"], 20);
    assert_eq!(diagnostics_props["failed_test_details"]["maxItems"], 20);
    assert_eq!(
        diagnostics_props["parser"]["enum"],
        json!(["structured_validation_parser"])
    );
    assert_eq!(
        diagnostics_props["failed_test_details_truncated"]["type"],
        "boolean"
    );
    assert_eq!(
        diagnostics_schema["additionalProperties"], false,
        "cargo_test diagnostics schema must close undeclared fields"
    );
    assert_eq!(
        diagnostics_props["test_summary"]["additionalProperties"], false,
        "cargo_test diagnostics.test_summary schema must close undeclared fields"
    );
    let summary_props = diagnostics_props["test_summary"]["properties"]
        .as_object()
        .expect("cargo_test diagnostics.test_summary properties");
    for field in ["passed", "failed", "ignored"] {
        assert!(
            summary_props.contains_key(field),
            "cargo_test diagnostics.test_summary missing {field}"
        );
    }
    for field in [
        "text",
        "format",
        "start_line",
        "limit",
        "total_lines",
        "returned_lines",
        "end_line",
        "has_more",
        "next_start_line",
        "sha256",
        "continuation",
    ] {
        assert!(
            has_output_field("read_file", field),
            "read_file missing {field}"
        );
    }
    for removed in ["content", "numbered_text"] {
        assert!(
            !has_output_field("read_file", removed),
            "read_file must not duplicate its primary text as {removed}"
        );
    }
    for field in [
        "backend",
        "result_mode",
        "effective_timeout_secs",
        "matches",
        "count",
        "files",
        "returned_file_count",
        "returned_match_count",
        "count_complete",
        "total_matches",
        "truncated",
        "truncation_reason",
        "continuation",
        "context_before",
        "context_after",
    ] {
        assert!(
            has_output_field("search_project_text", field),
            "search_project_text missing {field}"
        );
    }
    for field in ["project", "path", "entries", "truncated"] {
        assert!(
            has_output_field("list_project_files", field),
            "list_project_files missing {field}"
        );
    }
    for field in [
        "schema_version",
        "project",
        "path",
        "deterministic",
        "project_types",
        "manifests",
        "key_files",
        "roots",
        "top_level",
        "suggested_next_reads",
        "scan",
        "warnings",
    ] {
        assert!(
            has_output_field("project_overview", field),
            "project_overview missing {field}"
        );
    }
    assert!(
        !output_schema_properties(&specs, "list_project_files").contains_key("count"),
        "list_project_files schema must not invent a count field absent from runtime output"
    );
    let file_entries = output_schema_property(&specs, "list_project_files", "entries");
    let file_entry_props = file_entries["items"]["properties"]
        .as_object()
        .expect("list_project_files entries item properties");
    for field in ["path", "kind"] {
        assert!(
            file_entry_props.contains_key(field),
            "list_project_files entry missing {field}"
        );
    }
    for field in [
        "job_id",
        "kind",
        "status",
        "project",
        "ssh_resource",
        "last_update_seq",
    ] {
        assert!(
            has_output_field("run_job", field),
            "run_job missing {field}"
        );
    }
    for field in [
        "already_finished",
        "already_stop_requested",
        "stop_request_accepted",
        "target_was_active_at_request",
        "terminal",
        "terminal_pending",
        "final_status",
        "stop_effect",
        "job_id",
        "project",
        "status_before",
        "status_after",
        "command_started",
        "ownership_basis",
    ] {
        assert!(
            has_output_field("stop_job", field),
            "stop_job missing {field}"
        );
    }
    for field in [
        "job_id",
        "project",
        "session_id",
        "ssh_resource",
        "status",
        "exit_code",
        "started_at",
        "ended_at",
        "error",
        "command_execution_state",
        "structured_execution",
        "command_preview_included",
        "active",
        "blocking_active",
        "terminal",
        "terminal_pending",
        "command_preview",
        "command_preview_truncated",
        "command_preview_max_chars",
        "command_preview_bounded",
    ] {
        assert!(
            has_output_field("job_status", field),
            "job_status missing {field}"
        );
    }
    for field in [
        "job_id",
        "session_id",
        "ssh_resource",
        "exit_code",
        "command_execution_state",
        "structured_execution",
        "stdout_tail",
        "stderr_tail",
        "stdout_lines",
        "stderr_lines",
        "stdout_truncated",
        "stderr_truncated",
        "log_delta_status",
        "stdout_delta_reset",
        "stderr_delta_reset",
        "cursor",
        "status",
        "executor",
        "cwd",
        "shell",
        "purpose",
        "command_summary",
        "detected_summary",
        "wait_outcome",
        "waited_ms",
        "changed",
        "terminal",
    ] {
        assert!(
            has_output_field("job_log", field),
            "job_log missing {field}"
        );
    }
    for field in ["jobs", "count", "truncated"] {
        assert!(
            has_output_field("list_jobs", field),
            "list_jobs missing {field}"
        );
    }
    let jobs_schema = output_schema_property(&specs, "list_jobs", "jobs");
    let jobs_description = jobs_schema["description"]
        .as_str()
        .expect("list_jobs jobs description")
        .to_lowercase();
    assert!(
        jobs_description.contains("bounded") && jobs_description.contains("never includes stdout"),
        "list_jobs jobs description must describe bounded metadata without stdout/stderr bodies: {jobs_description}"
    );
    let job_summary_props = jobs_schema["items"]["properties"]
        .as_object()
        .expect("list_jobs item properties");
    for field in [
        "job_id",
        "kind",
        "status",
        "project",
        "session_id",
        "ssh_resource",
        "executor",
        "created_at",
        "started_at",
        "ended_at",
        "exit_code",
        "command_execution_state",
        "structured_execution",
    ] {
        assert!(
            job_summary_props.contains_key(field),
            "list_jobs summary missing {field}"
        );
    }
    for forbidden in ["stdout", "stderr"] {
        assert!(
            !job_summary_props.contains_key(forbidden),
            "list_jobs summary schema must not expose {forbidden} bodies"
        );
    }
    for field in [
        "job_id",
        "session_id",
        "ssh_resource",
        "exit_code",
        "command_execution_state",
        "structured_execution",
        "stdout_tail",
        "stderr_tail",
        "stdout_lines",
        "stderr_lines",
        "stdout_truncated",
        "stderr_truncated",
        "log_delta_status",
        "stdout_delta_reset",
        "stderr_delta_reset",
        "cursor",
        "status",
        "executor",
        "cwd",
        "shell",
        "purpose",
        "command_summary",
        "detected_summary",
        "wait_outcome",
        "waited_ms",
        "changed",
        "terminal",
    ] {
        assert!(
            has_output_field("job_log", field),
            "job_log missing {field}"
        );
    }
    for field in ["stdout_tail", "stderr_tail"] {
        let description = output_schema_property(&specs, "job_log", field)["description"]
            .as_str()
            .expect("job_log stream description")
            .to_lowercase();
        assert!(
            description.contains("bounded"),
            "job_log {field} description must describe bounded tail text: {description}"
        );
    }
    assert_eq!(
        output_schema_property(&specs, "job_log", "log_delta_status")["enum"],
        serde_json::json!(["baseline", "delta", "unchanged", "reset"])
    );
    assert_eq!(
        output_schema_property(&specs, "job_log", "observation_token")["maxLength"],
        webcodex_core::job_observation::MAX_JOB_OBSERVATION_TOKEN_LEN
    );
    let cursor_description = output_schema_property(&specs, "job_log", "cursor")["description"]
        .as_str()
        .expect("job_log cursor description")
        .to_lowercase();
    assert!(
        cursor_description.contains("cursor") && cursor_description.contains("bounded"),
        "job_log cursor must describe bounded continuation metadata: {cursor_description}"
    );
    for field in [
        "path",
        "exists",
        "missing",
        "bytes",
        "sha256",
        "mime_type",
        "modified_at",
    ] {
        assert!(
            has_output_field("read_project_artifact_metadata", field),
            "read_project_artifact_metadata missing {field}"
        );
    }
    for field in [
        "path",
        "file_bytes",
        "offset",
        "bytes_returned",
        "content_base64",
        "next_offset",
        "truncated",
        "eof",
    ] {
        assert!(
            has_output_field("read_project_artifact", field),
            "read_project_artifact missing {field}"
        );
    }
    let upload_progress_fields = [
        "path",
        "upload_id",
        "received_bytes",
        "next_offset",
        "expected_bytes",
        "expected_sha256",
        "committed",
    ];
    for field in upload_progress_fields {
        assert!(
            has_output_field("artifact_upload_begin", field),
            "artifact_upload_begin missing {field}"
        );
        assert!(
            has_output_field("artifact_upload_chunk", field),
            "artifact_upload_chunk missing {field}"
        );
    }
    for field in [
        "path",
        "upload_id",
        "bytes",
        "received_bytes",
        "expected_bytes",
        "expected_sha256",
        "sha256",
        "committed",
    ] {
        assert!(
            has_output_field("artifact_upload_finish", field),
            "artifact_upload_finish missing {field}"
        );
    }
    for field in [
        "path",
        "upload_id",
        "received_bytes",
        "aborted",
        "temp_file_removed",
        "sidecar_removed",
        "final_file_touched",
        "final_file_exists",
        "changed_path_details",
    ] {
        assert!(
            has_output_field("artifact_upload_abort", field),
            "artifact_upload_abort missing {field}"
        );
    }
    for field in [
        "service",
        "version",
        "build",
        "auth_enabled",
        "configured_public_url",
        "effective_config",
        "agents",
        "projects",
        "jobs",
        "tools",
        "authority",
        "quic",
    ] {
        assert!(
            has_output_field("runtime_status", field),
            "runtime_status missing {field}"
        );
    }
    for field in ["projects", "count", "recommended_for_smoke"] {
        assert!(
            has_output_field("list_projects", field),
            "list_projects missing {field}"
        );
    }
}

#[test]
fn project_onboarding_output_schemas_include_result_metadata_fields() {
    let specs = registered_tool_specs();

    for field in [
        "id",
        "agent_project_id",
        "client_id",
        "name",
        "path",
        "description",
        "project_record_path",
        "projects_config_path",
        "created_config",
        "overwritten",
        "allow_patch",
    ] {
        assert!(
            output_schema_properties(&specs, "register_project").contains_key(field),
            "register_project missing {field}"
        );
    }

    for field in [
        "id",
        "agent_project_id",
        "client_id",
        "name",
        "path",
        "description",
        "project_record_path",
        "projects_config_path",
        "created_directory",
        "created_config",
        "overwritten",
        "allow_patch",
        "template",
        "git_initialized",
    ] {
        assert!(
            output_schema_properties(&specs, "create_project").contains_key(field),
            "create_project missing {field}"
        );
    }

    for tool in ["register_project", "create_project"] {
        let props = output_schema_properties(&specs, tool);
        for forbidden in [
            "token",
            "secret",
            "env",
            "stdout",
            "stderr",
            "command",
            "file_content",
            "content",
        ] {
            assert!(
                !props.contains_key(forbidden),
                "{tool} output schema must not advertise {forbidden}"
            );
        }

        let descriptions = output_schema_description_text(props);
        for phrase in [
            "result metadata",
            "does not include file content",
            "does not expose environment, token, or secret values",
            "does not bypass authorization, permission, allowed-root, or runner path policy",
        ] {
            assert!(
                descriptions.contains(phrase),
                "{tool} output schema descriptions should mention {phrase}: {descriptions}"
            );
        }

        for field in ["path", "project_record_path", "projects_config_path"] {
            let description = output_schema_property(&specs, tool, field)["description"]
                .as_str()
                .expect("path-like field description")
                .to_lowercase();
            assert!(
                description.contains("result metadata path")
                    && description.contains("not file content"),
                "{tool} {field} description must describe metadata path only: {description}"
            );
        }

        for field in ["created_config", "overwritten"] {
            let description = output_schema_property(&specs, tool, field)["description"]
                .as_str()
                .expect("outcome field description")
                .to_lowercase();
            assert!(
                description.contains("result outcome metadata"),
                "{tool} {field} description must describe outcome metadata: {description}"
            );
        }
    }

    let created_directory_description =
        output_schema_property(&specs, "create_project", "created_directory")["description"]
            .as_str()
            .expect("created_directory description")
            .to_lowercase();
    assert!(
        created_directory_description.contains("result outcome metadata"),
        "create_project created_directory description must describe outcome metadata: {created_directory_description}"
    );

    let template_description = output_schema_property(&specs, "create_project", "template")
        ["description"]
        .as_str()
        .expect("template description")
        .to_lowercase();
    assert!(
        template_description.contains("does not change")
            && template_description.contains("template behavior"),
        "create_project template description must not imply behavior changes: {template_description}"
    );

    let git_description = output_schema_property(&specs, "create_project", "git_initialized")
        ["description"]
        .as_str()
        .expect("git_initialized description")
        .to_lowercase();
    assert!(
        git_description.contains("does not change") && git_description.contains("git-init"),
        "create_project git_initialized description must not imply behavior changes: {git_description}"
    );
}

#[test]
fn cleanup_tool_output_schemas_include_metadata_fields() {
    let specs = registered_tool_specs();

    for field in ["restored_paths", "command_result"] {
        assert!(
            output_schema_properties(&specs, "git_restore_paths").contains_key(field),
            "git_restore_paths missing {field}"
        );
    }
    for field in ["discarded_untracked_paths", "command_result"] {
        assert!(
            output_schema_properties(&specs, "discard_untracked").contains_key(field),
            "discard_untracked missing {field}"
        );
    }

    let restored = output_schema_property(&specs, "git_restore_paths", "restored_paths");
    assert_eq!(restored["type"], "array");
    assert_eq!(restored["items"]["type"], "string");

    let discarded =
        output_schema_property(&specs, "discard_untracked", "discarded_untracked_paths");
    assert_eq!(discarded["type"], "array");
    assert_eq!(discarded["items"]["type"], "string");
}

#[test]
fn cleanup_output_schemas_describe_result_metadata_only() {
    let specs = registered_tool_specs();

    for tool in ["git_restore_paths", "discard_untracked"] {
        let props = output_schema_properties(&specs, tool);
        for forbidden in [
            "content",
            "file_content",
            "stdout",
            "stderr",
            "stdin",
            "env",
            "token",
            "secret",
            "command",
            "shell_command",
        ] {
            assert!(
                !props.contains_key(forbidden),
                "{tool} output schema must not advertise {forbidden}"
            );
        }

        let description = output_schema_property(&specs, tool, "command_result")["description"]
            .as_str()
            .unwrap_or("")
            .to_lowercase();
        for phrase in [
            "fixed git cleanup",
            "result metadata",
            "not a general shell-execution interface",
        ] {
            assert!(
                description.contains(phrase),
                "{tool} command_result description should mention {phrase}: {description}"
            );
        }
    }
}

#[test]
fn write_project_file_output_schema_include_metadata_fields() {
    let specs = registered_tool_specs();

    // The removed legacy edit tools (`replace_in_file` and friends) are no
    // longer known tools, so they have no public ToolSpec/output schema.
    // Only the visible whole-file write tool's metadata schema is asserted
    // here.

    for field in [
        "path",
        "created",
        "overwritten",
        "bytes_written",
        "sha256",
        "changed",
        "state_changed",
        "execution_state",
        "error_kind",
        "failure_kind",
        "recovery_action",
        "retry_guidance",
        "error",
    ] {
        assert!(
            output_schema_properties(&specs, "write_project_file").contains_key(field),
            "write_project_file missing {field}"
        );
    }
    assert!(!output_schema_properties(&specs, "write_project_file").contains_key("warning"));
    assert_eq!(
        output_schema_property(&specs, "write_project_file", "bytes_written")["type"],
        "integer"
    );
    assert!(
        output_schema_property(&specs, "write_project_file", "state_changed")["anyOf"].is_array()
    );
    assert_eq!(
        output_schema_property(&specs, "write_project_file", "execution_state")["enum"],
        json!(["not_started", "completed", "outcome_unknown"])
    );
}

#[test]
fn cleanup_and_compatibility_write_output_schemas_do_not_advertise_broad_exfiltration() {
    let specs = registered_tool_specs();

    for tool in [
        "git_restore_paths",
        "discard_untracked",
        "write_project_file",
    ] {
        let props = output_schema_properties(&specs, tool);
        for forbidden in [
            "content",
            "file_content",
            "stdout",
            "stderr",
            "stdin",
            "env",
            "environment",
            "token",
            "secret",
            "old",
            "new",
            "command",
            "shell_command",
        ] {
            assert!(
                !props.contains_key(forbidden),
                "{tool} output schema must not advertise {forbidden}"
            );
        }
    }

    for tool in ["write_project_file"] {
        let descriptions = output_schema_description_text(output_schema_properties(&specs, tool));
        for phrase in [
            "result metadata",
            "does not include file content",
            "not a shell-execution interface",
            "does not expose environment, token, or secret values",
        ] {
            assert!(
                descriptions.contains(phrase),
                "{tool} output schema descriptions should mention {phrase}: {descriptions}"
            );
        }
    }
}

fn default_output_schema_field_names() -> BTreeSet<&'static str> {
    BTreeSet::from([
        "session_hint",
        "permission",
        "recovery_kind",
        "recovery_tool",
    ])
}

#[test]
fn model_facing_output_schemas_do_not_publish_recorder_only_telemetry() {
    let specs = registered_tool_specs();
    for spec in &specs {
        let serialized = serde_json::to_string(&spec.output_schema).unwrap();
        for field in ["session_recorded", "session_event_id"] {
            assert!(
                !serialized.contains(&format!("\"{field}\"")),
                "{} still publishes recorder-only {field}",
                spec.name
            );
        }
    }

    for tool in [
        "read_file",
        "read_files",
        "search_project_texts",
        "apply_patch",
        "cargo_check",
    ] {
        let fields = output_schema_field_names(spec_named(&specs, tool));
        assert!(
            !fields.contains("session_id"),
            "{tool} still publishes synthetic recorder session_id"
        );
    }

    assert!(
        output_schema_field_names(spec_named(&specs, "update_session_context"))
            .contains("session_id"),
        "business Workflow Session tools must retain their business session_id"
    );
}

fn output_schema_field_names(spec: &ToolSpec) -> BTreeSet<&str> {
    let mut fields = BTreeSet::new();
    collect_envelope_output_fields(&spec.output_schema, &mut fields);
    assert!(
        !fields.is_empty(),
        "{} output schema properties or variants",
        spec.name
    );
    fields
}

fn collect_envelope_output_fields<'a>(schema: &'a Value, fields: &mut BTreeSet<&'a str>) {
    if let Some(output) = schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get("output"))
    {
        collect_output_variant_fields(output, fields);
    }
    for keyword in ["oneOf", "anyOf", "allOf"] {
        if let Some(variants) = schema.get(keyword).and_then(Value::as_array) {
            for variant in variants {
                collect_envelope_output_fields(variant, fields);
            }
        }
    }
}

fn collect_output_variant_fields<'a>(schema: &'a Value, fields: &mut BTreeSet<&'a str>) {
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        fields.extend(properties.keys().map(String::as_str));
    }
    for keyword in ["oneOf", "anyOf", "allOf"] {
        if let Some(variants) = schema.get(keyword).and_then(Value::as_array) {
            for variant in variants {
                collect_output_variant_fields(variant, fields);
            }
        }
    }
    for keyword in ["then", "else"] {
        if let Some(branch) = schema.get(keyword) {
            collect_output_variant_fields(branch, fields);
        }
    }
}

fn output_schema_properties<'a>(
    specs: &'a [ToolSpec],
    name: &str,
) -> &'a serde_json::Map<String, Value> {
    let spec = spec_named(specs, name);
    spec.output_schema["properties"]["output"]["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("{} output schema properties", spec.name))
}

fn output_schema_property<'a>(specs: &'a [ToolSpec], name: &str, field: &str) -> &'a Value {
    output_schema_properties(specs, name)
        .get(field)
        .unwrap_or_else(|| panic!("{name} missing output field {field}"))
}

fn output_schema_description_text(props: &serde_json::Map<String, Value>) -> String {
    props
        .values()
        .filter_map(|schema| schema["description"].as_str())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[test]
fn validation_summary_schema_exposes_optional_recoverable_assertion_label_only() {
    let schema = output_schema_for_tool("validation_summary");
    let event = &schema["properties"]["output"]["properties"]["validation"]["properties"]["events"]
        ["items"];
    let properties = event["properties"].as_object().unwrap();
    let assertion = &properties["assertion_name"];
    assert_eq!(assertion["type"], "string");
    assert_eq!(assertion["minLength"], 1);
    assert_eq!(
        assertion["maxLength"],
        webcodex_core::workflow_session_contract::MAX_MODEL_VALIDATION_ASSERTION_NAME_CHARS
    );
    let required = event["required"].as_array().unwrap();
    assert!(!required.iter().any(|field| field == "assertion_name"));
    for hidden in ["expected_failure", "expected_failure_kind"] {
        assert!(
            !properties.contains_key(hidden),
            "validation event schema must not expose internal expectation field {hidden}"
        );
    }
}

#[test]
fn finish_coding_task_output_schema_describes_ledger_validation_summary() {
    let schema = output_schema_for_tool("finish_coding_task");
    let output_props = schema["properties"]["output"]["properties"]
        .as_object()
        .unwrap();
    assert!(
        output_props.contains_key("permissions"),
        "finish_coding_task output schema should include permissions"
    );
    assert!(
        output_props.contains_key("tool_failures"),
        "finish_coding_task output schema should include classified tool failures"
    );
    assert!(
        output_props.contains_key("summary_only"),
        "finish_coding_task output schema should include summary_only for compact output"
    );
    assert!(
        output_props.contains_key("review_evidence"),
        "finish_coding_task output schema should include review_evidence"
    );
    assert!(
        !output_props.contains_key("verdict"),
        "finish_coding_task output schema should omit legacy verdict"
    );
    assert!(
        !output_props.contains_key("finish_verdict"),
        "finish_coding_task output schema should omit finish_verdict alias"
    );
    for field in [
        "task_outcome",
        "evidence_history",
        "evidence_integrity",
        "informational_notes",
    ] {
        assert!(
            output_props.contains_key(field),
            "finish_coding_task output schema should include {field}"
        );
    }
    assert_outcome_model_schema_fields(output_props);
    assert!(
        output_props.contains_key("suggested_next_actions"),
        "finish_coding_task output schema should include top-level suggested_next_actions"
    );
    assert_permission_summary_schema_fields(&output_props["permissions"]);
    assert_job_lifecycle_summary_schema_fields(&output_props["jobs"]);
    assert_review_evidence_schema_fields(&output_props["review_evidence"]);
    let description = schema["properties"]["output"]["properties"]["validation"]["description"]
        .as_str()
        .unwrap();
    let description = description.to_lowercase();
    for phrase in [
        "validation closeout evidence",
        "full closeout",
        "historical",
        "resolved",
        "unresolved",
        "stable identity",
        "summary_only",
        "final status/reason",
        "success/failure counts",
        "zero-test integrity flag",
    ] {
        assert!(
            description.contains(phrase),
            "validation output schema should mention {phrase}: {description}"
        );
    }
    for forbidden in [
        "backward-compatible",
        "first_diagnostic",
        "first_failed_test",
        "failed_tests,",
    ] {
        assert!(
            !description.contains(forbidden),
            "validation output schema must not mention removed compatibility phrase {forbidden}: {description}"
        );
    }
    let review_description = output_props["review_evidence"]["description"]
        .as_str()
        .unwrap()
        .to_lowercase();
    for phrase in [
        "full-closeout",
        "ledger-derived",
        "non-cargo review evidence",
        "omitted from summary_only",
        "internally in canonical task_outcome",
        "does not include file contents",
    ] {
        assert!(
            review_description.contains(phrase),
            "finish review_evidence schema should mention {phrase}: {review_description}"
        );
    }
    let suggested_description = output_props["suggested_next_actions"]["description"]
        .as_str()
        .unwrap()
        .to_lowercase();
    for phrase in [
        "top-level",
        "final closeout actions",
        "summary_only",
        "task outcome",
        "evidence integrity",
    ] {
        assert!(
            suggested_description.contains(phrase),
            "finish suggested_next_actions schema should mention {phrase}: {suggested_description}"
        );
    }
}

#[test]
fn session_handoff_summary_schema_exposes_ledger_validation_summary() {
    let specs = registered_tool_specs();
    let spec = spec_named(&specs, "session_handoff_summary");
    let input_props = spec.input_schema["properties"].as_object().unwrap();
    assert!(
        input_props.contains_key("include_validation"),
        "session_handoff_summary input schema should include include_validation"
    );
    assert!(
        input_props.contains_key("summary_only"),
        "session_handoff_summary input schema should include summary_only"
    );

    let schema = output_schema_for_tool("session_handoff_summary");
    let output_props = schema["properties"]["output"]["properties"]
        .as_object()
        .unwrap();
    assert!(
        output_props.contains_key("validation"),
        "session_handoff_summary output schema should include validation"
    );
    assert!(
        output_props.contains_key("review_evidence"),
        "session_handoff_summary output schema should include review_evidence"
    );
    assert!(
        output_props.contains_key("permissions"),
        "session_handoff_summary output schema should include permissions"
    );
    assert!(
        output_props.contains_key("tool_failures"),
        "session_handoff_summary output schema should include classified tool failures"
    );
    assert!(
        output_props.contains_key("expected_failed_tool_calls"),
        "session_handoff_summary output schema should include expected failed tool calls"
    );
    assert!(
        output_props.contains_key("unexpected_failed_tool_calls"),
        "session_handoff_summary output schema should include unexpected failed tool calls"
    );
    assert!(
        output_props.contains_key("expectation_mismatches"),
        "session_handoff_summary output schema should include expectation mismatches"
    );
    for field in [
        "task_outcome",
        "evidence_history",
        "evidence_integrity",
        "informational_notes",
    ] {
        assert!(
            output_props.contains_key(field),
            "session_handoff_summary output schema should include {field}"
        );
    }
    assert_outcome_model_schema_fields(output_props);
    assert_permission_summary_schema_fields(&output_props["permissions"]);
    assert_job_lifecycle_summary_schema_fields(&output_props["jobs"]);
    assert_review_evidence_schema_fields(&output_props["review_evidence"]);
    let description = output_props["validation"]["description"]
        .as_str()
        .unwrap()
        .to_lowercase();
    for phrase in [
        "ledger-derived",
        "validation-like tool-call summary",
        "status/reason",
        "does not include stdout/stderr",
        "structured diagnostics",
        "bounded validation metadata",
        "parser version 3",
        "canonical diagnostics",
        "failed_test_details",
        "no root-cause inference",
        "parser.available remains false when session ledger events lack those fields",
        "latest_status",
        "historical_failures",
    ] {
        assert!(
            description.contains(phrase),
            "handoff validation output schema should mention {phrase}: {description}"
        );
    }
    for forbidden in [
        "backward-compatible",
        "first_diagnostic",
        "first_failed_test",
        "failed_tests,",
    ] {
        assert!(
            !description.contains(forbidden),
            "handoff validation output schema must not mention removed compatibility phrase {forbidden}: {description}"
        );
    }
    let review_description = output_props["review_evidence"]["description"]
        .as_str()
        .unwrap()
        .to_lowercase();
    for phrase in [
        "ledger-derived",
        "non-cargo review evidence",
        "summary_only",
        "read/search/diff/workspace/hygiene",
        "bounded tools",
        "does not include file contents",
        "does not change validation.status",
    ] {
        assert!(
            review_description.contains(phrase),
            "handoff review_evidence schema should mention {phrase}: {review_description}"
        );
    }
}

fn assert_permission_summary_schema_fields(schema: &Value) {
    let props = schema["properties"].as_object().unwrap();
    for field in [
        "manual_approved_count",
        "auto_approved_count",
        "total_approved_count",
    ] {
        assert!(props.contains_key(field), "permissions missing {field}");
    }
    assert!(
        !props.contains_key("approved_count"),
        "permission schema must not retain the approved_count compatibility alias"
    );
}

fn assert_job_lifecycle_summary_schema_fields(schema: &Value) {
    let props = schema["properties"].as_object().unwrap();
    for field in [
        "active_count",
        "running_count",
        "stop_requested_count",
        "terminal_pending_count",
        "blocking_active_count",
        "nonblocking_active_count",
        "warnings",
    ] {
        assert!(props.contains_key(field), "jobs summary missing {field}");
    }
}

fn assert_review_evidence_schema_fields(schema: &Value) {
    let props = schema["properties"].as_object().unwrap();
    for field in [
        "available",
        "total",
        "read_only_inspection_count",
        "search_count",
        "diff_review_count",
        "workspace_review_count",
        "hygiene_review_count",
        "tools",
    ] {
        assert!(props.contains_key(field), "review_evidence missing {field}");
    }
    assert_eq!(props["tools"]["type"], "array");
    assert_eq!(props["tools"]["items"]["type"], "string");
}

fn assert_outcome_model_schema_fields(output_props: &serde_json::Map<String, Value>) {
    assert_eq!(
        output_props["task_outcome"]["properties"]["status"]["enum"],
        json!(["pass", "warn", "fail"])
    );
    assert_eq!(
        output_props["evidence_history"]["properties"]["status"]["enum"],
        json!(["clean", "mixed_resolved", "mixed_unresolved", "failed"])
    );
    assert_eq!(
        output_props["evidence_integrity"]["properties"]["status"]["enum"],
        json!(["clean", "warning", "error"])
    );
    assert_eq!(output_props["informational_notes"]["type"], "array");
}
