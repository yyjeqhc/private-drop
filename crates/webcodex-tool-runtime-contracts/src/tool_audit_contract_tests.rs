use super::*;
use crate::tool_call_test_support::sample_tool_args;
use serde_json::json;
use webcodex_tool_contracts::registered_tool_specs;

#[test]
fn unknown_retired_and_noncanonical_request_names_fail_closed() {
    let arguments = json!({
        "project": "agent:test:demo",
        "body": "PRIVATE_UNKNOWN_BODY",
        "arguments": {"opaque": "PRIVATE_ARGUMENTS"},
        "audit_policy": "SessionEvidence"
    });
    for name in [
        "unregistered_tool",
        "start_coding_task",
        "list_agents",
        "ReadFile",
        "read_file ",
    ] {
        assert_eq!(
            session_log_arguments_for_tool_request(name, &arguments),
            Value::Null,
            "{name}"
        );
    }
}

#[test]
fn absent_malformed_policy_and_serialization_failure_omit_requests() {
    let arguments = json!({"body": "PRIVATE_BODY", "project": "agent:test:demo"});
    let mut invalid = lookup_tool_definition("memory_read").unwrap().audit;
    const INVALID_FIELDS: &[AuditField] = &[AuditField::new("", "body", AuditValue::Copy)];
    invalid.request.fields = INVALID_FIELDS;
    for stage in [AuditStage::Request, AuditStage::Typed] {
        assert_eq!(project_arguments(None, &arguments, stage), Value::Null);
        assert_eq!(
            project_arguments(Some(&invalid), &arguments, stage),
            Value::Null
        );
    }
    let policy = &lookup_tool_definition("memory_read").unwrap().audit;
    let serialization_error = serde_json::from_str::<Value>("PRIVATE_NOT_JSON").unwrap_err();
    assert_eq!(
        project_serialized_call(Some(policy), Err(serialization_error)),
        Value::Null
    );
    assert_eq!(
        project_serialized_call(Some(policy), Ok(json!("PRIVATE_BODY"))),
        Value::Null
    );
    assert_eq!(
        project_serialized_call(Some(policy), Ok(json!({"params": "PRIVATE_BODY"}))),
        Value::Null
    );
}

#[test]
fn malformed_execution_context_and_nonobject_requests_never_pass_through() {
    let summary = session_log_arguments_for_tool_request(
        "update_session_context",
        &json!({
            "session_id": "wc_sess_test",
            "execution_context": {"cwd": 42, "unexpected": "PRIVATE_CONTEXT_BODY"}
        }),
    );
    assert_eq!(summary["execution_context"], Value::Null);
    assert!(!summary.to_string().contains("PRIVATE_CONTEXT_BODY"));
    for value in [
        Value::Null,
        json!("PRIVATE_BODY"),
        json!(["PRIVATE_BODY"]),
        json!(42),
    ] {
        assert_eq!(
            session_log_arguments_for_tool_request("memory_read", &value),
            Value::Null
        );
    }
}

#[test]
fn canonical_request_audit_never_mutates_business_arguments_or_calls() {
    for spec in registered_tool_specs() {
        let arguments = sample_tool_args(&spec.name);
        let before = arguments.clone();
        let _summary = session_log_arguments_for_tool_request(&spec.name, &arguments);
        assert_eq!(arguments, before, "{} raw input changed", spec.name);
        let call = ToolCall::from_tool_name(&spec.name, arguments).unwrap();
        let before = serde_json::to_value(&call).unwrap();
        let _summary = call.session_log_arguments();
        assert_eq!(
            serde_json::to_value(&call).unwrap(),
            before,
            "{} typed input changed",
            spec.name
        );
    }
}

#[test]
fn compatibility_parser_alias_uses_canonical_typed_audit_only() {
    let arguments = json!({"client_id": "PRIVATE_RUNNER_FILTER", "summary_only": true});
    let alias = ToolCall::from_tool_name("list_agents", arguments.clone()).unwrap();
    let canonical = ToolCall::from_tool_name("list_runners", arguments.clone()).unwrap();
    assert_eq!(alias.tool_name(), "list_runners");
    assert_eq!(
        alias.session_log_arguments(),
        canonical.session_log_arguments()
    );
    assert!(!alias
        .session_log_arguments()
        .to_string()
        .contains("PRIVATE_RUNNER_FILTER"));
    assert_eq!(
        session_log_arguments_for_tool_request("list_agents", &arguments),
        Value::Null
    );
}

#[test]
fn historically_omitted_typed_summaries_stay_omitted() {
    for name in [
        "run_detached_process",
        "computer_read_clipboard",
        "get_session_assignment",
        "list_project_tracked_files",
    ] {
        let call = ToolCall::from_tool_name(name, sample_tool_args(name)).unwrap();
        assert_eq!(call.session_log_arguments(), json!({}), "{name}");
    }
}

#[test]
fn declared_request_rules_never_copy_undeclared_fields() {
    for spec in registered_tool_specs() {
        let mut arguments = sample_tool_args(&spec.name);
        arguments["undeclared_body"] = json!({"nested": "PRIVATE_UNDECLARED_BODY"});
        let summary = session_log_arguments_for_tool_request(&spec.name, &arguments);
        assert!(
            !summary.to_string().contains("PRIVATE_UNDECLARED_BODY"),
            "{}",
            spec.name
        );
    }
}

#[test]
fn raw_fallback_tightening_keeps_metadata_but_not_native_paths_tokens_or_edit_bodies() {
    for name in ["register_project", "create_project"] {
        let summary = session_log_arguments_for_tool_request(
            name,
            &json!({
                "client_id": "runner-a", "id": "demo", "name": "Demo",
                "path": "/private/native/project", "description": "PRIVATE_DESCRIPTION"
            }),
        );
        assert_eq!(
            summary,
            json!({"client_id":"runner-a", "id":"demo", "name":"Demo"})
        );
    }
    let summary = session_log_arguments_for_tool_request(
        "job_log",
        &json!({
            "job_id":"job_123", "tail_lines":40, "wait_secs":1,
            "after_observation_token":"PRIVATE_JOB_TOKEN"
        }),
    );
    assert_eq!(
        summary,
        json!({"job_id":"job_123", "tail_lines":40, "wait_secs":1})
    );
    for name in ["plugin_tool", "ssh_resource"] {
        let summary = session_log_arguments_for_tool_request(
            name,
            &json!({
                "arguments":{"body":"PRIVATE_PLUGIN_BODY"}, "binding":"PRIVATE_BINDING",
                "target":{"host":"PRIVATE_HOST", "password":"PRIVATE_CREDENTIAL"}
            }),
        );
        assert_eq!(summary, json!({}));
    }
    let summary = session_log_arguments_for_tool_request(
        "apply_text_edits",
        &json!({
            "project":"demo", "dry_run":true,
            "changes":[{"kind":"create", "path":"src/new.rs", "content":"PRIVATE_EDIT_BODY"}]
        }),
    );
    assert_eq!(summary["change_count"], 1);
    assert_eq!(summary["paths"], json!(["src/new.rs"]));
    assert_eq!(summary["dry_run"], true);
    assert!(!summary.to_string().contains("PRIVATE_EDIT_BODY"));
    let summary = session_log_arguments_for_tool_request(
        "git_diff",
        &json!({"project":"demo", "args":["PRIVATE_ARG"]}),
    );
    assert_eq!(summary, json!({"project":"demo", "args_count":1}));
}

#[test]
fn typed_option_presence_and_checkpoint_defaults_preserve_existing_contract() {
    let process =
        ToolCall::from_tool_name("run_process", json!({"project":"demo", "executable":"git"}))
            .unwrap();
    assert_eq!(process.session_log_arguments()["stdin_present"], false);
    let observation =
        ToolCall::from_tool_name("observe_jobs", json!({"items":[{"job_id":"job_123"}]})).unwrap();
    assert_eq!(observation.session_log_arguments()["token_count"], 0);
    let checkpoint =
        ToolCall::from_tool_name("workspace_checkpoint_create", json!({"project":"demo"})).unwrap();
    assert_eq!(checkpoint.session_log_arguments()["kind"], "snapshot");
    assert_eq!(checkpoint.session_log_arguments()["note_present"], false);
    assert_eq!(
        checkpoint.session_log_arguments()["validation_status"],
        "unknown"
    );
}
