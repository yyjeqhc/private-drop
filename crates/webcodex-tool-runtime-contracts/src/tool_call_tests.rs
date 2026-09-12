//! Tool Call parser, wire, accessor, and audit-contract tests.

use crate::tool_call_test_support::*;
use crate::*;
use serde_json::{json, Value};
use webcodex_tool_contracts::{
    is_known_tool_name, is_model_hidden_tool_name, known_tool_names, lookup_tool_definition,
    model_hidden_tool_names, registered_tool_specs,
};
use webcodex_workflow_session as sessions;

#[test]
fn from_tool_name_parses_unit_tools_without_arguments() {
    for name in [
        "list_tools",
        "computer_list_targets",
        "list_projects",
        "list_runners",
        "runtime_status",
    ] {
        let call = ToolCall::from_tool_name(name, Value::Null).unwrap_or_else(|e| panic!("{}", e));
        assert!(
            matches!(
                call,
                ToolCall::ListTools { .. }
                    | ToolCall::ComputerListTargets
                    | ToolCall::ListProjects { .. }
                    | ToolCall::ListRunners { .. }
                    | ToolCall::RuntimeStatus { .. }
            ),
            "unit tool {} should parse",
            name
        );
    }
}

#[test]
fn from_tool_name_parses_unit_tools_with_empty_object() {
    let call = ToolCall::from_tool_name("list_tools", json!({})).unwrap();
    assert!(matches!(call, ToolCall::ListTools { .. }));
}

#[test]
fn runner_config_tools_parse_closed_contracts_and_keep_governance_split() {
    use webcodex_tool_contracts::{
        RunnerCapabilityRequirement, ToolApprovalPolicy, ToolEffect, ToolIdempotency, ToolRisk,
    };

    let check =
        ToolCall::from_tool_name("runner_config_check", json!({"client_id": "special"})).unwrap();
    assert!(matches!(
        check,
        ToolCall::RunnerConfigCheck { ref client_id } if client_id == "special"
    ));

    let reload = ToolCall::from_tool_name(
        "runner_config_reload",
        json!({"client_id": "special", "expected_generation": 7}),
    )
    .unwrap();
    assert!(matches!(
        reload,
        ToolCall::RunnerConfigReload {
            ref client_id,
            expected_generation: 7,
        } if client_id == "special"
    ));

    for (name, args) in [
        (
            "runner_config_check",
            json!({"client_id": "special", "path": "/tmp/forbidden"}),
        ),
        (
            "runner_config_reload",
            json!({"client_id": "special", "expected_generation": 7, "path": "/tmp/forbidden"}),
        ),
    ] {
        assert!(ToolCall::from_tool_name(name, args).is_err(), "{name}");
    }
    assert!(ToolCall::from_tool_name("runner_config", json!({"action": "check"})).is_err());
    assert!(lookup_tool_definition("describe_config").is_none());

    let check_definition = lookup_tool_definition("runner_config_check").unwrap();
    assert_eq!(check_definition.metadata.effect, ToolEffect::Observe);
    assert_eq!(check_definition.metadata.risk, ToolRisk::Read);
    assert_eq!(check_definition.metadata.approval, ToolApprovalPolicy::None);
    assert_eq!(
        check_definition.metadata.idempotency,
        ToolIdempotency::PureRead
    );
    assert_eq!(
        check_definition.runner_capability,
        Some(RunnerCapabilityRequirement::RunnerConfigControl)
    );
    assert_eq!(
        check_definition.metadata.authority,
        webcodex_core::authority::ToolAuthorityPolicy::Require(
            webcodex_core::authority::SCOPE_RUNTIME_READ
        )
    );

    let reload_definition = lookup_tool_definition("runner_config_reload").unwrap();
    assert_eq!(reload_definition.metadata.effect, ToolEffect::Mutate);
    assert_eq!(reload_definition.metadata.risk, ToolRisk::RunControl);
    assert_eq!(
        reload_definition.metadata.approval,
        ToolApprovalPolicy::Standard
    );
    assert_eq!(
        reload_definition.metadata.idempotency,
        ToolIdempotency::FencedReplay
    );
    assert_eq!(
        reload_definition.runner_capability,
        Some(RunnerCapabilityRequirement::RunnerConfigControl)
    );
    assert_eq!(
        reload_definition.metadata.authority,
        webcodex_core::authority::ToolAuthorityPolicy::Require(
            webcodex_core::authority::SCOPE_RUNNER_MANAGE
        )
    );
}

#[test]
fn ssh_resource_parses_as_canonical_gateway_with_closed_action_vocabulary() {
    let list = ToolCall::from_tool_name(
        "ssh_resource",
        json!({"action": "list", "runner": "special"}),
    )
    .unwrap();
    assert!(matches!(
        list,
        ToolCall::SshResource(SshResourceToolCall {
            ref action,
            runner: Some(ref runner),
            ..
        }) if action == "list" && runner == "special"
    ));

    let register = ToolCall::from_tool_name(
        "ssh_resource",
        json!({
            "action": "register",
            "binding": "wc_sbind_0123456789abcdef0123456789abcdef",
            "name": "spe",
            "target": "root@spe",
            "default_cwd": "/root/git"
        }),
    )
    .unwrap();
    assert!(matches!(
        register,
        ToolCall::SshResource(SshResourceToolCall {
            ref action,
            name: Some(ref name),
            target: Some(ref target),
            ..
        }) if action == "register" && name == "spe" && target == "root@spe"
    ));

    for invalid in [
        json!({"action": "probe", "runner": "special"}),
        json!({"action": "list", "runner": "special", "unknown": true}),
    ] {
        assert!(ToolCall::from_tool_name("ssh_resource", invalid).is_err());
    }
}

#[test]
fn list_agents_live_ingress_is_rejected_without_second_definition() {
    let error = ToolCall::from_tool_name(
        "list_agents",
        json!({"client_id": "special", "include_projects": false}),
    )
    .expect_err("retired list_agents must not remain a live ingress alias");
    assert!(error.contains("unknown tool 'list_agents'"), "{error}");

    let call = ToolCall::from_tool_name(
        "list_runners",
        json!({"client_id": "special", "include_projects": false}),
    )
    .unwrap();
    assert!(matches!(
        call,
        ToolCall::ListRunners {
            client_id: Some(ref client_id),
            include_projects: Some(false),
            ..
        } if client_id == "special"
    ));
    assert!(lookup_tool_definition("list_agents").is_none());
    assert!(lookup_tool_definition("list_runners").is_some());
}

#[test]
fn from_tool_name_parses_bounded_list_tools_options() {
    let call = ToolCall::from_tool_name(
        "list_tools",
        json!({
            "category": "artifact",
            "features": "artifact_upload",
            "summary_only": true,
            "limit": 4
        }),
    )
    .unwrap();
    match call {
        ToolCall::ListTools {
            category,
            features,
            summary_only,
            limit,
        } => {
            assert_eq!(category.as_deref(), Some("artifact"));
            assert_eq!(features.as_deref(), Some("artifact_upload"));
            assert!(summary_only);
            assert_eq!(limit, Some(4));
        }
        other => panic!("expected ListTools, got {:?}", other),
    }
}

#[test]
fn from_tool_name_records_and_strips_testing_metadata_before_parsing() {
    let (call, metadata) = ToolCall::from_tool_name_with_recorder_metadata(
        "list_jobs",
        json!({
            "status": "failed",
            "expected_failure": true,
            "expected_failure_kind": "job_not_found",
            "assertion_name": "missing job negative path"
        }),
    )
    .unwrap();
    assert!(matches!(call, ToolCall::ListJobs { .. }));
    assert!(metadata.expectation.expected_failure);
    assert_eq!(
        metadata.expectation.expected_failure_kind.as_deref(),
        Some("job_not_found")
    );
    assert_eq!(
        metadata.expectation.assertion_name.as_deref(),
        Some("missing job negative path")
    );
}

#[test]
fn from_tool_name_records_public_result_expectations_before_parsing() {
    let (call, metadata) = ToolCall::from_tool_name_with_recorder_metadata(
        "run_process",
        json!({
            "project": "demo",
            "executable": "git",
            "args": ["merge-base", "--is-ancestor", "a", "b"],
            "result_expectation": "observe",
            "accepted_exit_codes": [1, 0, 1]
        }),
    )
    .unwrap();
    assert!(matches!(call, ToolCall::RunProcess { .. }));
    assert!(!metadata.expectation.expected_failure);
    assert_eq!(
        metadata.expectation.result_expectation.as_deref(),
        Some("observe")
    );
    assert_eq!(metadata.expectation.accepted_exit_codes, vec![0, 1]);

    let (call, metadata) = ToolCall::from_tool_name_with_recorder_metadata(
        "cargo_test",
        json!({
            "project": "demo",
            "filter": "reproduce_bug",
            "result_expectation": "failure"
        }),
    )
    .unwrap();
    assert!(matches!(call, ToolCall::CargoTest { .. }));
    assert!(metadata.expectation.expected_failure);
    assert_eq!(
        metadata.expectation.result_expectation.as_deref(),
        Some("failure")
    );
    assert!(metadata.expectation.accepted_exit_codes.is_empty());
}

#[test]
fn from_tool_name_rejects_unsafe_result_expectation_combinations() {
    let invalid = [
        (
            "run_process",
            json!({
                "project": "demo",
                "executable": "git",
                "result_expectation": "maybe"
            }),
        ),
        (
            "run_process",
            json!({
                "project": "demo",
                "executable": "git",
                "result_expectation": "failure",
                "accepted_exit_codes": [0, 1]
            }),
        ),
        (
            "cargo_test",
            json!({
                "project": "demo",
                "accepted_exit_codes": [0, 1]
            }),
        ),
        (
            "cargo_fmt",
            json!({
                "project": "demo",
                "result_expectation": "failure"
            }),
        ),
        (
            "cargo_fmt",
            json!({
                "project": "demo",
                "check": false,
                "result_expectation": "observe"
            }),
        ),
        (
            "list_jobs",
            json!({
                "result_expectation": "observe"
            }),
        ),
    ];

    for (tool, arguments) in invalid {
        let error = ToolCall::from_tool_name_with_recorder_metadata(tool, arguments).unwrap_err();
        assert!(
            error.contains("result_expectation")
                || error.contains("accepted_exit_codes")
                || error.contains("result expectation"),
            "{tool}: {error}"
        );
    }
}

#[test]
fn from_tool_name_rejects_removed_failure_kind_alias_as_tool_input() {
    let error = ToolCall::from_tool_name_with_recorder_metadata(
        "list_jobs",
        json!({
            "expected_failure": true,
            "test_expect_failure_kind": "job_not_found",
            "assertion_name": "removed alias"
        }),
    )
    .unwrap_err();
    assert!(error.contains("test_expect_failure_kind"), "{error}");
    assert!(error.contains("unknown field"), "{error}");
}

#[test]
fn artifact_upload_followup_tools_missing_path_error_is_actionable() {
    for name in [
        "artifact_upload_chunk",
        "artifact_upload_finish",
        "artifact_upload_abort",
    ] {
        let err =
            ToolCall::from_tool_name(name, json!({"upload_id": "wc_upload_test_1"})).unwrap_err();
        assert!(
            err.contains("path is required")
                && err.contains("artifact_upload_begin")
                && err.contains("bind upload_id"),
            "{name}: {err}"
        );
    }
}

#[test]
fn from_tool_name_parses_read_project_artifact_metadata_allow_missing() {
    let call = ToolCall::from_tool_name(
        "read_project_artifact_metadata",
        json!({
            "project": "agent:demo:smoke",
            "path": "artifacts/smoke/missing.artifact",
            "allow_missing": true
        }),
    )
    .unwrap();

    match call {
        ToolCall::ReadProjectArtifactMetadata {
            project,
            path,
            allow_missing,
            ..
        } => {
            assert_eq!(project, "agent:demo:smoke");
            assert_eq!(path, "artifacts/smoke/missing.artifact");
            assert_eq!(allow_missing, Some(true));
        }
        other => panic!("expected ReadProjectArtifactMetadata, got {:?}", other),
    }
}

#[test]
fn from_tool_name_parses_run_shell_with_required_fields() {
    let call = ToolCall::from_tool_name(
        "run_shell",
        json!({"project": "demo", "command": "echo hi"}),
    )
    .unwrap();
    match call {
        ToolCall::RunShell {
            project,
            command,
            timeout_secs,
            cwd,
            ..
        } => {
            assert_eq!(project, "demo");
            assert_eq!(command, "echo hi");
            assert_eq!(timeout_secs, None);
            assert_eq!(cwd, None);
        }
        other => panic!("expected RunShell, got {:?}", other),
    }
}

#[test]
fn from_tool_name_parses_run_shell_with_optional_fields() {
    let call = ToolCall::from_tool_name(
        "run_shell",
        json!({"project": "demo", "command": "ls", "timeout_secs": 180, "cwd": "sub"}),
    )
    .unwrap();
    match call {
        ToolCall::RunShell {
            project,
            command,
            timeout_secs,
            cwd,
            ..
        } => {
            assert_eq!(project, "demo");
            assert_eq!(command, "ls");
            assert_eq!(timeout_secs, Some(180));
            assert_eq!(cwd, Some("sub".to_string()));
        }
        other => panic!("expected RunShell, got {:?}", other),
    }
}

#[test]
fn structured_validation_sync_wait_parser_enforces_lifecycle_bounds() {
    for (name, arguments) in [
        (
            "cargo_check",
            json!({"project": "demo", "timeout_secs": 600, "sync_wait_secs": 1}),
        ),
        (
            "cargo_test",
            json!({"project": "demo", "timeout_secs": 600, "sync_wait_secs": 60}),
        ),
        (
            "go_test",
            json!({"project": "demo", "timeout_secs": 60, "sync_wait_secs": 60}),
        ),
        (
            "cargo_fmt",
            json!({"project": "demo", "check": true, "timeout_secs": 60, "sync_wait_secs": 60}),
        ),
        (
            "cargo_test",
            json!({"project": "demo", "timeout_secs": 600, "sync_wait_secs": 61}),
        ),
        (
            "go_test",
            json!({"project": "demo", "timeout_secs": 30, "sync_wait_secs": 31}),
        ),
    ] {
        ToolCall::from_tool_name(name, arguments)
            .unwrap_or_else(|error| panic!("{name} valid sync wait should parse: {error}"));
    }

    for (name, arguments) in [
        (
            "cargo_check",
            json!({"project": "demo", "timeout_secs": 600, "sync_wait_secs": 0}),
        ),
        (
            "cargo_fmt",
            json!({"project": "demo", "check": false, "timeout_secs": 60, "sync_wait_secs": 1}),
        ),
        (
            "cargo_fmt",
            json!({"project": "demo", "timeout_secs": 60, "sync_wait_secs": 1}),
        ),
    ] {
        let error = ToolCall::from_tool_name(name, arguments)
            .expect_err("zero or semantically unavailable sync wait must fail closed");
        assert!(error.contains("sync_wait_secs"), "{name}: {error}");
    }
}

#[test]
fn from_tool_name_parses_structured_run_process_boundaries() {
    let call = ToolCall::from_tool_name(
        "run_process",
        json!({
            "project": "demo",
            "executable": "git",
            "args": ["status", "--porcelain", "two words", "$(literal)"],
            "cwd": ".",
            "stdin": "input\n",
            "timeout_secs": 60,
            "sync_wait_secs": 45,
            "purpose": "diagnostic",
            "session_id": "wc_sess_process"
        }),
    )
    .unwrap();
    match call {
        ToolCall::RunProcess {
            project,
            executable,
            args,
            cwd,
            stdin,
            timeout_secs,
            sync_wait_secs,
            purpose,
            session_id,
        } => {
            assert_eq!(project, "demo");
            assert_eq!(executable, "git");
            assert_eq!(
                args,
                ["status", "--porcelain", "two words", "$(literal)"].map(str::to_string)
            );
            assert_eq!(cwd.as_deref(), Some("."));
            assert_eq!(stdin.as_deref(), Some("input\n"));
            assert_eq!(timeout_secs, Some(60));
            assert_eq!(sync_wait_secs, Some(45));
            assert_eq!(purpose, Some(ExecutionPurpose::Diagnostic));
            assert_eq!(session_id.as_deref(), Some("wc_sess_process"));
        }
        other => panic!("expected RunProcess, got {other:?}"),
    }

    let empty = ToolCall::from_tool_name(
        "run_process",
        json!({
            "project": "demo",
            "executable": "git",
            "stdin": null
        }),
    )
    .unwrap();
    match empty {
        ToolCall::RunProcess { args, stdin, .. } => {
            assert!(args.is_empty());
            assert!(stdin.is_none());
        }
        other => panic!("expected RunProcess, got {other:?}"),
    }
}

#[test]
fn from_tool_name_rejects_retired_job_status_and_job_log() {
    for (name, args) in [
        ("job_status", json!({"job_id": "abc"})),
        ("job_log", json!({"job_id": "abc", "offset": 10})),
    ] {
        let error = ToolCall::from_tool_name(name, args).unwrap_err();
        assert!(error.contains("unknown tool"), "{name}: {error}");
    }
}

#[test]
fn from_tool_name_parses_stop_job_with_default_confirmation_false() {
    let call =
        ToolCall::from_tool_name("stop_job", json!({"project": "demo", "job_id": "abc"})).unwrap();
    match call {
        ToolCall::StopJob {
            project,
            job_id,
            confirm,
            session_id,
        } => {
            assert_eq!(project, "demo");
            assert_eq!(job_id, "abc");
            assert!(!confirm);
            assert!(session_id.is_none());
        }
        other => panic!("expected StopJob, got {:?}", other),
    }

    let call = ToolCall::from_tool_name(
        "stop_job",
        json!({"project": "demo", "job_id": "abc", "session_id": "wc_sess_x", "confirm": true}),
    )
    .unwrap();
    assert!(matches!(
        call,
        ToolCall::StopJob {
            ref project,
            ref job_id,
            ref session_id,
            confirm: true,
        } if project == "demo" && job_id == "abc" && session_id.as_deref() == Some("wc_sess_x")
    ));
}

#[test]
fn from_tool_name_rejects_retired_inspection_tools_and_parses_retained_git_tools() {
    let error =
        ToolCall::from_tool_name("read_file", json!({"project": "demo", "path": "README.md"}))
            .unwrap_err();
    assert!(error.contains("unknown tool"), "{error}");

    let call = ToolCall::from_tool_name("git_status", json!({"project": "demo"})).unwrap();
    assert!(matches!(call, ToolCall::GitStatus { .. }));

    for name in ["git_diff", "git_diff_summary"] {
        let error = ToolCall::from_tool_name(name, json!({"project": "demo"})).unwrap_err();
        assert!(error.contains("unknown tool"), "{name}: {error}");
    }

    let call = ToolCall::from_tool_name(
        "apply_unified_diff",
        json!({"project": "demo", "diff": "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-a\n+b\n"}),
    )
    .unwrap();
    assert!(matches!(call, ToolCall::ApplyUnifiedDiff { .. }));

    let call =
        ToolCall::from_tool_name("run_job", json!({"project": "demo", "command": "make"})).unwrap();
    assert!(matches!(call, ToolCall::RunJob { .. }));
}

#[test]
fn continuation_endpoint_rotation_has_canonical_and_legacy_tool_names() {
    let args = json!({
        "agent_id": format!("wc_dagent_{}", "a".repeat(32)),
        "host": "ChatGPT",
        "client_attachment_id": "window-a",
        "idempotency_key": "rotate-endpoint-1"
    });

    let canonical =
        ToolCall::from_tool_name("rotate_agent_continuation_endpoint", args.clone()).unwrap();
    assert_eq!(canonical.tool_name(), "rotate_agent_continuation_endpoint");
    assert!(matches!(
        canonical,
        ToolCall::RotateAgentContinuationEndpoint { .. }
    ));

    let legacy = ToolCall::from_tool_name("attach_agent_endpoint", args).unwrap();
    assert_eq!(legacy.tool_name(), "attach_agent_endpoint");
    assert!(matches!(legacy, ToolCall::AttachAgentEndpoint { .. }));
}

#[test]
fn from_tool_name_rejects_unknown_tool_name() {
    let err = ToolCall::from_tool_name("not_a_tool", Value::Null).unwrap_err();
    assert!(err.contains("not_a_tool"));
}

#[test]
fn from_tool_name_rejects_missing_required_field() {
    let err = ToolCall::from_tool_name("run_shell", json!({"command": "echo"})).unwrap_err();
    assert!(
        err.contains("project"),
        "error should mention missing field: {}",
        err
    );

    let err = ToolCall::from_tool_name("job_tail", json!({})).unwrap_err();
    assert!(err.contains("job_id"));
}

#[test]
fn from_tool_name_rejects_wrong_field_type() {
    let err = ToolCall::from_tool_name("run_shell", json!({"project": 123, "command": "echo"}))
        .unwrap_err();
    assert!(!err.is_empty());
}

#[test]
fn from_tool_name_error_includes_tool_name() {
    let err = ToolCall::from_tool_name("run_shell", json!({})).unwrap_err();
    assert!(err.contains("run_shell"));
}

#[test]
fn tool_call_project_accessor_covers_project_tool_specs() {
    for spec in registered_tool_specs() {
        let args = sample_tool_args(&spec.name);
        let expected_project = match spec.name.as_str() {
            "start_session" => {
                // start_session project is task association metadata, not an
                // execution target exposed by the project accessor.
                None
            }
            "unregister_project" => {
                // unregister_project carries an exact lifecycle target, but it
                // must bypass generic project pre-resolution so a terminal
                // already_unregistered outcome remains representable.
                None
            }
            _ => args
                .get("project")
                .and_then(Value::as_str)
                .map(str::to_string),
        };
        let call = ToolCall::from_tool_name(&spec.name, args)
            .unwrap_or_else(|e| panic!("{} should deserialize: {}", spec.name, e));
        assert_eq!(
            call.project(),
            expected_project.as_deref(),
            "{} ToolCall::project() mismatch",
            spec.name
        );
    }

    // start_session's optional project is task association metadata, not an
    // execution target used for authorization or kernel project reporting.
    let start_session =
        ToolCall::from_tool_name("start_session", json!({"project": "agent:oe:private-drop"}))
            .unwrap();
    assert_eq!(start_session.project(), None);

    // session_handoff_summary's optional project IS exposed by project()
    // when provided, so the kernel can report it and authorize the workspace
    // git inspection path.
    let handoff = ToolCall::from_tool_name(
        "session_handoff_summary",
        json!({"session_id": "wc_sess_x", "project": "agent:oe:private-drop"}),
    )
    .unwrap();
    assert_eq!(handoff.project(), Some("agent:oe:private-drop"));
}

#[test]
fn tool_call_session_id_accessor_covers_session_tool_specs() {
    for spec in registered_tool_specs() {
        if spec.input_schema["properties"].get("session_id").is_none() {
            continue;
        }
        if spec.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == "session_id")
        {
            continue;
        }
        let call = ToolCall::from_tool_name(&spec.name, sample_tool_args_with_session(&spec.name))
            .unwrap_or_else(|e| panic!("{} should deserialize: {}", spec.name, e));
        let expected = if spec.name == "list_jobs" {
            // list_jobs.session_id is an exact metadata filter over the
            // already-authorized Job set. It deliberately does not opt into
            // generic Workflow Session lookup/recording, which would turn a
            // foreign or missing filter value into an existence oracle.
            None
        } else {
            Some("wc_sess_accessor")
        };
        assert_eq!(
            call.session_id(),
            expected,
            "{} ToolCall::session_id() mismatch",
            spec.name
        );
    }
}

#[test]
fn from_tool_name_unknown_tool_lists_available_tools_and_hint() {
    let err = ToolCall::from_tool_name("definitely_not_a_tool", Value::Null).unwrap_err();
    assert!(err.contains("definitely_not_a_tool"));
    assert!(
        err.contains("listRuntimeTools") || err.contains("list_tools"),
        "unknown-tool error should hint at discovery: {}",
        err
    );
    // Should list at least a couple of known tool names.
    assert!(err.contains("show_changes"));
    assert!(err.contains("apply_unified_diff"));
    // Must not leak secret/config artifacts.
    let lower = err.to_lowercase();
    for forbidden in [
        "token",
        "authorization",
        "runner.toml",
        "agent.toml",
        "webcodex.env",
        "secret",
    ] {
        assert!(
            !lower.contains(forbidden),
            "unknown-tool error must not leak '{}': {}",
            forbidden,
            err
        );
    }
}

#[test]
fn known_tool_names_matches_spec_count() {
    let specs = registered_tool_specs();
    for spec in &specs {
        assert!(
            is_known_tool_name(&spec.name),
            "{} spec must be known to ToolCall",
            spec.name
        );
        assert!(
            !is_model_hidden_tool_name(&spec.name),
            "{} must not be model-hidden when exposed in registered tool specs",
            spec.name
        );
    }
    assert_eq!(
        specs.len(),
        known_tool_names().count() - model_hidden_tool_names().count(),
        "registered tool specs should cover every model-visible known runtime tool; \
         hidden tools are parser-known but carry no ToolSpec"
    );
    // Every known name must be recognized (i.e. must NOT yield the
    // "unknown tool" error). Unit tools parse with null args; non-unit
    // tools fail with a missing-field error, which is still a recognition
    // success (the variant matched).
    for name in known_tool_names() {
        assert!(
            is_known_tool_name(name),
            "known name '{}' not recognized by is_known_tool_name",
            name
        );
        let result = ToolCall::from_tool_name(name, Value::Null);
        match result {
            Ok(_) => {}
            Err(e) => {
                assert!(
                    !e.contains("unknown tool"),
                    "known tool '{}' was treated as unknown: {}",
                    name,
                    e
                );
            }
        }
    }
    // An unknown name must still produce the unknown-tool error.
    let err = ToolCall::from_tool_name("not_a_real_tool", Value::Null).unwrap_err();
    assert!(err.contains("unknown tool"));
    assert!(
        !err.contains("run_codex"),
        "unknown-tool guidance must not advertise hidden tools: {}",
        err
    );
}

#[test]
fn from_tool_name_parses_runtime_status() {
    let call = ToolCall::from_tool_name("runtime_status", Value::Null).unwrap();
    assert!(matches!(
        call,
        ToolCall::RuntimeStatus {
            compact: false,
            summary_only: false,
            client_id: None,
        }
    ));
    // Also accepts an empty object.
    let call = ToolCall::from_tool_name("runtime_status", json!({})).unwrap();
    assert!(matches!(
        call,
        ToolCall::RuntimeStatus {
            compact: false,
            summary_only: false,
            client_id: None,
        }
    ));
    let call = ToolCall::from_tool_name(
        "runtime_status",
        json!({"compact": true, "summary_only": true}),
    )
    .unwrap();
    assert!(matches!(
        call,
        ToolCall::RuntimeStatus {
            compact: true,
            summary_only: true,
            client_id: None,
        }
    ));
}

#[test]
fn work_on_project_parses_path_source_and_rejects_ambiguous_sources() {
    let work = ToolCall::from_tool_name(
        "work_on_project",
        json!({
            "client_id": "runner-1",
            "path": "/root/git/example",
            "instruction": "implement it"
        }),
    )
    .unwrap();
    assert!(work.project().is_none());
    let work_audit = work.session_log_arguments();
    assert_eq!(work_audit["path_source_requested"], true);
    assert!(work_audit.get("path").is_none());
    assert!(!work_audit.to_string().contains("/root/git/example"));

    for path in [
        r"C:\repo",
        "c:/repo",
        r"\\?\C:\repo",
        r"\\server\share\repo",
    ] {
        ToolCall::from_tool_name(
            "work_on_project",
            json!({"client_id": "runner-1", "path": path, "instruction": "implement it"}),
        )
        .unwrap_or_else(|error| panic!("work_on_project rejected {path:?}: {error}"));
    }

    for (tool, arguments) in [
        (
            "work_on_project",
            json!({"project": "agent:x:y", "path": "/tmp/y", "instruction": "x"}),
        ),
        (
            "work_on_project",
            json!({"client_id": "x", "instruction": "x"}),
        ),
        (
            "work_on_project",
            json!({"client_id": "x", "path": "relative/repo", "instruction": "x"}),
        ),
        (
            "work_on_project",
            json!({"client_id": "x", "path": r"\repo", "instruction": "x"}),
        ),
        (
            "work_on_project",
            json!({"client_id": "", "path": "/tmp/y", "instruction": "x"}),
        ),
    ] {
        let error = ToolCall::from_tool_name(tool, arguments).unwrap_err();
        assert!(
            error.contains("conflicting fields")
                || error.contains("missing ")
                || error.contains("must be an absolute")
                || error.contains("must not be empty"),
            "{error}"
        );
    }
}

#[test]
fn session_execution_context_parses_as_strongly_typed_replacement() {
    let ssh = ToolCall::from_tool_name(
        "update_session_context",
        json!({
            "project": "agent:oe:demo",
            "session_id": "wc_sess_context01",
            "execution_context": {
                "default_cwd": "/opt/webcodex-edge",
                "resource": "tmp"
            }
        }),
    )
    .unwrap();
    assert!(matches!(
        ssh,
        ToolCall::UpdateSessionContext {
            execution_context: sessions::SessionExecutionContext {
                default_cwd: Some(ref cwd),
                resource: Some(ref resource),
                ..
            },
            ..
        } if cwd == "/opt/webcodex-edge" && resource == "tmp"
    ));

    let clear = ToolCall::from_tool_name(
        "update_session_context",
        json!({
            "project": "agent:oe:demo",
            "session_id": "wc_sess_context01",
            "execution_context": {}
        }),
    )
    .unwrap();
    assert!(matches!(
        clear,
        ToolCall::UpdateSessionContext {
            ref project,
            ref session_id,
            execution_context: sessions::SessionExecutionContext {
                default_cwd: None,
                default_shell: None,
                ..
            },
        } if project == "agent:oe:demo" && session_id == "wc_sess_context01"
    ));

    for invalid in [
        json!({
            "project": "agent:oe:demo",
            "session_id": "wc_sess_context01",
            "execution_context": {"env": {"TOKEN": "secret"}}
        }),
        json!({
            "project": "agent:oe:demo",
            "session_id": "wc_sess_context01",
            "execution_context": {"default_shell": "zsh"}
        }),
    ] {
        let error = ToolCall::from_tool_name("update_session_context", invalid).unwrap_err();
        assert!(error.contains("invalid arguments"));
    }
}

#[test]
fn from_tool_name_parses_finish_coding_task_workspace_projection_flag() {
    let call = ToolCall::from_tool_name(
        "finish_coding_task",
        json!({
            "project": "agent:client:demo",
            "session_id": "wc_sess_demo",
            "summary_only": true,
            "include_workspace": true,
            "include_validation_summary": true,
            "include_hygiene": true,
            "include_handoff": true,
            "include_diff": false
        }),
    )
    .unwrap();

    match call {
        ToolCall::FinishCodingTask {
            project,
            session_id,
            summary_only,
            include_diff,
            include_workspace,
            include_hygiene,
            include_handoff,
            include_validation_summary,
        } => {
            assert_eq!(project, "agent:client:demo");
            assert_eq!(session_id, "wc_sess_demo");
            assert!(summary_only);
            assert_eq!(include_diff, Some(false));
            assert_eq!(include_workspace, Some(true));
            assert_eq!(include_hygiene, Some(true));
            assert_eq!(include_handoff, Some(true));
            assert_eq!(include_validation_summary, Some(true));
        }
        other => panic!("expected finish_coding_task, got {other:?}"),
    }
}

#[test]
fn observe_session_messages_tool_call_and_audit_are_bounded() {
    let raw_token = "wsm1:wc_sess_demo:1";
    let call = ToolCall::from_tool_name(
        "observe_session_messages",
        json!({
            "session_id": "wc_sess_demo",
            "after_observation_token": raw_token,
            "wait_secs": 7,
            "limit": 25
        }),
    )
    .unwrap();
    match &call {
        ToolCall::ObserveSessionMessages {
            session_id,
            after_observation_token,
            wait_secs,
            limit,
        } => {
            assert_eq!(session_id, "wc_sess_demo");
            assert_eq!(after_observation_token.as_deref(), Some(raw_token));
            assert_eq!(*wait_secs, Some(7));
            assert_eq!(*limit, Some(25));
        }
        other => panic!("expected ObserveSessionMessages, got {other:?}"),
    }
    assert_eq!(call.tool_name(), "observe_session_messages");
    assert_eq!(
        call.session_log_arguments(),
        json!({
            "session_id": "wc_sess_demo",
            "wait_secs": 7,
            "limit": 25,
            "token_present": true
        })
    );
    let output_audit = crate::tool_audit::session_log_result_for_tool(
        "observe_session_messages",
        &json!({
            "success": true,
            "session_id": "wc_sess_demo",
            "messages": [{"message": "secret body"}],
            "observation_token": raw_token,
            "changed": true,
            "history_lost": false,
            "has_more": true,
            "wait_outcome": "immediate"
        }),
    );
    assert_eq!(output_audit["message_count"], 1);
    assert_eq!(output_audit["changed"], true);
    assert_eq!(output_audit["has_more"], true);
    assert!(output_audit.get("messages").is_none());
    assert!(output_audit.get("observation_token").is_none());
    assert!(!output_audit.to_string().contains("secret body"));
    assert!(!output_audit.to_string().contains(raw_token));

    let oversized = ToolCall::from_tool_name(
        "observe_session_messages",
        json!({
            "session_id": "wc_sess_demo",
            "after_observation_token": "x".repeat(193)
        }),
    );
    assert!(oversized.is_err());
}

#[test]
fn project_overview_tool_call_parses() {
    let call = ToolCall::from_tool_name(
        "project_overview",
        json!({
            "project": "agent:client:demo",
            "path": "crates/example",
            "max_depth": 3,
            "limit": 120
        }),
    )
    .unwrap();

    match call {
        ToolCall::ProjectOverview {
            project,
            path,
            max_depth,
            limit,
            ..
        } => {
            assert_eq!(project, "agent:client:demo");
            assert_eq!(path.as_deref(), Some("crates/example"));
            assert_eq!(max_depth, Some(3));
            assert_eq!(limit, Some(120));
        }
        other => panic!("expected ProjectOverview, got {other:?}"),
    }

    let audit_call = ToolCall::from_tool_name(
        "project_overview",
        json!({
            "project": "agent:client:demo",
            "path": "src",
            "max_depth": 2,
            "limit": 40
        }),
    )
    .unwrap();
    assert_eq!(
        audit_call.session_log_arguments(),
        json!({
            "project": "agent:client:demo",
            "path": "src",
            "max_depth": 2,
            "limit": 40
        })
    );
}

#[test]
fn from_tool_name_parses_phase_a_tools() {
    let call = ToolCall::from_tool_name(
        "list_project_files",
        json!({"project": "demo", "limit": 50, "offset": 75}),
    )
    .unwrap();
    match call {
        ToolCall::ListProjectFiles {
            project,
            path,
            limit,
            offset,
            ..
        } => {
            assert_eq!(project, "demo");
            assert_eq!(path, None);
            assert_eq!(limit, Some(50));
            assert_eq!(offset, Some(75));
        }
        other => panic!("expected ListProjectFiles, got {:?}", other),
    }

    let error = ToolCall::from_tool_name(
        "search_project_text",
        json!({"project": "demo", "pattern": "fn main"}),
    )
    .unwrap_err();
    assert!(error.contains("unknown tool"), "{error}");

    let call = ToolCall::from_tool_name(
        "search_project_texts",
        json!({
            "project": "demo",
            "queries": [{
                "pattern": "fn main",
                "limit": 5,
                "context_before": 3,
                "context_after": 8,
                "include_globs": ["**/*.rs"],
                "exclude_globs": ["vendor/**"],
                "result_mode": "count",
                "timeout_secs": 45
            }]
        }),
    )
    .unwrap();
    assert!(matches!(
        call,
        ToolCall::SearchProjectTexts { ref project, ref queries, .. }
            if project == "demo"
                && queries.len() == 1
                && queries[0].result_mode == Some(SearchResultMode::Count)
                && queries[0].timeout_secs == Some(45)
    ));

    for name in ["job_status", "job_log", "git_diff", "git_diff_summary"] {
        let error = ToolCall::from_tool_name(name, json!({})).unwrap_err();
        assert!(error.contains("unknown tool"), "{name}: {error}");
    }

    // list_jobs has only optional fields; null arguments must still parse.
    let call = ToolCall::from_tool_name("list_jobs", Value::Null).unwrap();
    assert!(matches!(
        call,
        ToolCall::ListJobs {
            limit: None,
            status: None,
            project: None,
            session_id: None,
        }
    ));
    let call =
        ToolCall::from_tool_name("list_jobs", json!({"limit": 3, "status": "running"})).unwrap();
    match call {
        ToolCall::ListJobs { limit, status, .. } => {
            assert_eq!(limit, Some(3));
            assert_eq!(status.as_deref(), Some("running"));
        }
        other => panic!("expected ListJobs, got {:?}", other),
    }

    let call =
        ToolCall::from_tool_name("job_tail", json!({"job_id": "abc", "tail_lines": 10})).unwrap();
    match call {
        ToolCall::JobTail {
            job_id,
            tail_lines,
            after_observation_token,
            wait_secs,
        } => {
            assert_eq!(job_id, "abc");
            assert_eq!(tail_lines, Some(10));
            assert_eq!(after_observation_token, None);
            assert_eq!(wait_secs, None);
        }
        other => panic!("expected JobTail, got {:?}", other),
    }
}

#[test]
fn from_tool_name_list_jobs_with_null_arguments_parses() {
    // Regression: a non-unit tool with all-optional fields must deserialize
    // when a caller passes `null` arguments (normalized to an empty object).
    let call = ToolCall::from_tool_name("list_jobs", Value::Null)
        .unwrap_or_else(|e| panic!("list_jobs with null args should parse: {}", e));
    assert!(matches!(call, ToolCall::ListJobs { .. }));
}

#[test]
fn from_tool_name_parses_unified_diff_and_cleanup_tools() {
    let unified = ToolCall::from_tool_name(
        "apply_unified_diff",
        json!({"project":"agent:c:p","diff":"diff","deny_sensitive_paths":true}),
    )
    .unwrap();
    assert!(matches!(
        unified,
        ToolCall::ApplyUnifiedDiff { project, diff, deny_sensitive_paths, .. }
            if project == "agent:c:p" && diff == "diff" && deny_sensitive_paths == Some(true)
    ));

    let delete = ToolCall::from_tool_name(
        "delete_project_files",
        json!({"project":"agent:c:p","paths":["tmp.txt"]}),
    )
    .unwrap();
    assert!(
        matches!(delete, ToolCall::DeleteProjectFiles { project, paths, .. } if project == "agent:c:p" && paths == vec!["tmp.txt"])
    );

    let restore = ToolCall::from_tool_name(
        "git_restore_paths",
        json!({"project":"agent:c:p","paths":["README.md"]}),
    )
    .unwrap();
    assert!(
        matches!(restore, ToolCall::GitRestorePaths { project, paths, .. } if project == "agent:c:p" && paths == vec!["README.md"])
    );

    let discard = ToolCall::from_tool_name(
        "discard_untracked",
        json!({"project":"agent:c:p","paths":["tmp.txt"]}),
    )
    .unwrap();
    assert!(
        matches!(discard, ToolCall::DiscardUntracked { project, paths, .. } if project == "agent:c:p" && paths == vec!["tmp.txt"])
    );
}

#[test]
fn from_tool_name_parses_apply_patch_and_rejects_retired_patch_helpers() {
    let patch = ToolCall::from_tool_name(
        "apply_patch",
        json!({
            "project": "agent:c:p",
            "patch": "*** Begin Patch\n*** Add File: new.txt\n+hello\n*** End Patch",
            "matching_mode": "exact_unique"
        }),
    )
    .expect("current apply_patch DSL tool must parse");
    assert!(matches!(
        patch,
        ToolCall::ApplyPatch { project, patch, dry_run, matching_mode, .. }
            if project == "agent:c:p"
                && patch.contains("*** Add File: new.txt")
                && dry_run.is_none()
                && matching_mode == Some(webcodex_core::apply_patch_shared::ApplyPatchMatchingMode::ExactUnique)
    ));

    for legacy_strict in [true, false] {
        let error = ToolCall::from_tool_name(
            "apply_patch",
            json!({
                "project": "agent:c:p",
                "patch": "*** Begin Patch\n*** Add File: legacy.txt\n+hello\n*** End Patch",
                "strict_matching": legacy_strict
            }),
        )
        .expect_err("legacy strict_matching must fail closed at current Server ingress");
        assert!(error.contains("strict_matching"), "{error}");
        assert!(error.contains("matching_mode"), "{error}");
    }

    for removed in ["apply_patch_checked", "validate_patch"] {
        let error = ToolCall::from_tool_name(removed, json!({"project":"agent:c:p"}))
            .expect_err("retired patch helper names must not parse");
        assert!(error.contains(removed), "{error}");
    }
}

#[test]
fn from_tool_name_parses_write_project_file() {
    let write = ToolCall::from_tool_name(
        "write_project_file",
        json!({
            "project": "agent:c:p",
            "path": "new.txt",
            "content": "hello"
        }),
    )
    .unwrap();
    assert!(matches!(
        write,
        ToolCall::WriteProjectFile { project, path, content, overwrite, expected_sha256, .. }
            if project == "agent:c:p"
            && path == "new.txt"
            && content == "hello"
            && overwrite.is_none()
            && expected_sha256.is_none()
    ));
}

#[test]
fn from_tool_name_rejects_retired_write_prefix_guard() {
    let error = ToolCall::from_tool_name(
        "write_project_file",
        json!({
            "project": "agent:c:p",
            "path": "existing.txt",
            "content": "replacement",
            "overwrite": true,
            "expected_sha256": "a".repeat(64),
            "expected_content_prefix": "legacy"
        }),
    )
    .expect_err("retired prefix guard must fail before dispatch");
    assert!(error.contains("expected_content_prefix"), "{error}");
    assert!(error.contains("no longer supported"), "{error}");
    assert!(error.contains("expected_sha256"), "{error}");
}

#[test]
fn from_tool_name_rejects_removed_legacy_edit_tools() {
    // The 7 legacy edit tools replaced by `apply_text_edits` are no longer
    // known ToolDefinitions, so `from_tool_name` must reject them with the
    // same unknown-tool error as any other unknown name.
    for name in [
        "replace_in_file",
        "replace_exact_block",
        "insert_before_pattern",
        "insert_after_pattern",
        "replace_line_range",
        "insert_at_line",
        "delete_line_range",
    ] {
        let err = ToolCall::from_tool_name(name, Value::Null).unwrap_err();
        assert!(err.contains("unknown tool"), "{name}: {err}");
    }
}

#[test]
fn from_tool_name_parses_project_management_tools() {
    let register = ToolCall::from_tool_name(
        "register_project",
        json!({
            "client_id":"oe",
            "id":"my-project",
            "name":"My Project",
            "path":"/root/git/my-project"
        }),
    )
    .unwrap();
    assert!(matches!(
        register,
        ToolCall::RegisterProject { ref client_id, ref id, ref name, ref path, .. }
            if client_id == "oe" && id == "my-project" && name == "My Project"
            && path == "/root/git/my-project"
    ));

    let revision = format!("sha256:{}", "a".repeat(64));
    let unregister = ToolCall::from_tool_name(
        "unregister_project",
        json!({
            "project":"agent:oe:my-project",
            "expected_revision": revision
        }),
    )
    .unwrap();
    assert!(matches!(
        unregister,
        ToolCall::UnregisterProject { ref project, ref expected_revision }
            if project == "agent:oe:my-project" && expected_revision == &revision
    ));
    assert_eq!(unregister.project(), None, "unregister must bypass generic project pre-resolution so already_unregistered remains representable");

    let create = ToolCall::from_tool_name(
        "create_project",
        json!({
            "client_id":"oe",
            "id":"hello",
            "name":"Hello",
            "path":"/root/git/hello",
            "template":"basic",
            "git_init":true
        }),
    )
    .unwrap();
    assert!(matches!(
        create,
        ToolCall::CreateProject { ref client_id, ref id, ref name, ref path, ref template, git_init, .. }
            if client_id == "oe" && id == "hello" && name == "Hello"
            && path == "/root/git/hello" && template.as_deref() == Some("basic")
            && git_init
    ));
}

#[test]
fn start_coding_task_uses_generic_unknown_tool_and_privacy_paths() {
    let error = ToolCall::from_tool_name("start_coding_task", json!({"project": "demo"}))
        .expect_err("retired start_coding_task must be an ordinary unknown tool");
    assert!(
        error.contains("unknown tool 'start_coding_task'"),
        "{error}"
    );

    let audit = crate::tool_audit::session_log_arguments_for_tool_request(
        "start_coding_task",
        &json!({
            "project": "agent:legacy:demo",
            "path": "/private/legacy/path",
            "prompt": "PRIVATE_PROMPT",
            "secret": "PRIVATE_SECRET"
        }),
    );
    assert_eq!(audit, json!({}));
}

#[test]
fn create_project_rejects_retired_managed_temporary_field() {
    let error = ToolCall::from_tool_name(
        "create_project",
        json!({
            "client_id":"oe",
            "id":"hello",
            "name":"Hello",
            "path":"/root/git/hello",
            "managed_temporary_project":true
        }),
    )
    .expect_err("retired managed-temporary field must not be model/API reachable");
    assert!(error.contains("managed_temporary_project"), "{error}");
}

#[test]
fn create_project_rejects_retired_allow_existing_empty_with_migration_hint() {
    let error = ToolCall::from_tool_name(
        "create_project",
        json!({
            "client_id":"oe",
            "id":"hello",
            "name":"Hello",
            "path":"/root/git/hello",
            "allow_existing_empty":true
        }),
    )
    .expect_err("retired empty-directory adoption field must fail closed");
    assert!(error.contains("allow_existing_empty"), "{error}");
    assert!(error.contains("adopt_existing_empty"), "{error}");
}

#[test]
fn agent_continuation_bind_parses_required_view_fence_and_omits_it_from_audit() {
    let binding_id = format!("wc_host_binding_{}", "a0".repeat(16));
    let mut args = json!({
        "agent_id": format!("wc_dagent_{}", "a".repeat(32)),
        "endpoint_id": format!("wc_endpoint_{}", "b".repeat(32)),
        "expected_controller_generation": 1,
        "binding_id": binding_id,
    });
    let call = ToolCall::from_tool_name("agent_continuation_bind", args.clone()).unwrap();
    assert!(
        matches!(&call, ToolCall::AgentContinuationBind { binding_id: parsed, .. } if parsed == &binding_id)
    );
    let audit =
        crate::tool_audit::session_log_arguments_for_tool_request("agent_continuation_bind", &args)
            .to_string();
    assert!(!audit.contains("binding_id"));
    assert!(!audit.contains(&binding_id));
    args.as_object_mut().unwrap().remove("binding_id");
    assert!(ToolCall::from_tool_name("agent_continuation_bind", args).is_err());
}
