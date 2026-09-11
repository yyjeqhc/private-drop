use super::*;
use crate::runner_protocol::{
    RunnerCapabilities, RunnerJobUpdateRequest, RunnerPollRequest, RunnerProjectSummary,
    RunnerRegisterRequest,
};
use crate::tool_runtime::{ObserveJobsItem, ToolCall};

fn tool<'a>(payload: &'a Value, name: &str) -> &'a Value {
    payload["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("missing {name} descriptor"))
}

fn presentation<'a>(call_result: &'a Value) -> &'a Value {
    &call_result["_meta"][super::super::presentation::MCP_PRESENTATION_META_KEY]
}

const RESULT_APP_TOOLS: [&str; 6] = [
    "list_jobs",
    "observe_jobs",
    "cargo_check",
    "cargo_test",
    "go_test",
    "validation_summary",
];
const UNBOUND_RESULT_APP_TOOLS: [&str; 5] = [
    "cargo_fmt",
    "run_shell",
    "run_process",
    "run_job",
    "finish_coding_task",
];

fn assert_presentation_strings_bounded(value: &Value) {
    match value {
        Value::String(text) => assert!(
            text.chars().count() <= super::super::presentation::MAX_MCP_PRESENTATION_TEXT_CHARS,
            "presentation text exceeded bound: {}",
            text.chars().count()
        ),
        Value::Array(values) => values.iter().for_each(assert_presentation_strings_bounded),
        Value::Object(values) => values
            .values()
            .for_each(assert_presentation_strings_bounded),
        _ => {}
    }
}

fn projected_result(tool_name: &str, success: bool, output: Value) -> Value {
    let canonical = ToolResult {
        success,
        output,
        error: (!success).then(|| "canonical validation failure".to_string()),
    };
    let mut framed = super::super::tools::mcp_runtime_tool_result(tool_name, false, canonical);
    let structured_before = framed["structuredContent"].clone();
    super::super::presentation::attach_result_app_presentation(tool_name, &mut framed);
    assert_eq!(framed["structuredContent"], structured_before);
    framed
}

async fn handle_with_server_apps_enabled(
    runtime: &ToolRuntime,
    request: JsonRpcRequest,
    auth: Option<&crate::auth::AuthContext>,
    server_mcp_apps_enabled: bool,
) -> McpOutcome {
    let protocol_era = super::super::inferred_protocol_era(&request);
    super::super::handle_mcp_request_with_lifecycle(
        runtime,
        None,
        request,
        auth,
        protocol_era,
        super::super::HostFileImportTrust::Untrusted,
        None,
        None,
        None,
        crate::config::mcp_compact_schemas_enabled(),
        server_mcp_apps_enabled,
        None,
    )
    .await
}

#[test]
fn result_tool_app_metadata_is_capability_scoped_compact_safe_and_merge_safe() {
    for compact in [false, true] {
        let enabled = mcp_tools_list_payload_with_compact_and_app(
            ModelSurface::FullOperatorRuntime,
            compact,
            true,
        );
        for name in RESULT_APP_TOOLS {
            assert!(super::super::presentation::tool_supports_result_app(name));
            assert_eq!(
                tool(&enabled, name)["_meta"]["ui"]["resourceUri"],
                MCP_RESULT_UI_RESOURCE_URI
            );
            assert!(tool(&enabled, name)["_meta"]
                .get("ui/resourceUri")
                .is_none());
        }
        for name in UNBOUND_RESULT_APP_TOOLS {
            assert!(!super::super::presentation::tool_supports_result_app(name));
            assert_ne!(
                tool(&enabled, name)
                    .pointer("/_meta/ui/resourceUri")
                    .and_then(Value::as_str),
                Some(MCP_RESULT_UI_RESOURCE_URI)
            );
        }

        let disabled = mcp_tools_list_payload_with_compact_and_app(
            ModelSurface::FullOperatorRuntime,
            compact,
            false,
        );
        for name in RESULT_APP_TOOLS {
            assert!(tool(&disabled, name).get("_meta").is_none());
        }
    }

    let mut existing = json!({
        "name": "future_job_tool",
        "_meta": {
            "openai/fileParams": ["file"],
            "ui": {"other": true}
        }
    });
    super::super::tools::attach_app_metadata(&mut existing, MCP_RESULT_UI_RESOURCE_URI);
    assert_eq!(existing["_meta"]["openai/fileParams"], json!(["file"]));
    assert_eq!(existing["_meta"]["ui"]["other"], true);
    assert_eq!(
        existing["_meta"]["ui"]["resourceUri"],
        MCP_RESULT_UI_RESOURCE_URI
    );
}

#[tokio::test]
async fn result_app_descriptor_and_resource_exposure_require_ui_operator_capability() {
    let runtime = test_runtime_with_surface(ModelSurface::FullOperatorRuntime);
    let ui_tools = handle_mcp_request(
        &runtime,
        rpc(
            "tools/list",
            Some(json!(3201)),
            mcp_2026_ui_params(json!({})),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(ui_tools) = ui_tools else {
        panic!("expected UI-capable tools/list");
    };
    for name in RESULT_APP_TOOLS {
        assert_eq!(
            tool(&ui_tools["result"], name)["_meta"]["ui"]["resourceUri"],
            MCP_RESULT_UI_RESOURCE_URI
        );
        assert!(tool(&ui_tools["result"], name)["_meta"]
            .get("ui/resourceUri")
            .is_none());
    }
    for name in UNBOUND_RESULT_APP_TOOLS {
        assert_ne!(
            tool(&ui_tools["result"], name)
                .pointer("/_meta/ui/resourceUri")
                .and_then(Value::as_str),
            Some(MCP_RESULT_UI_RESOURCE_URI)
        );
    }

    let plain_tools = handle_mcp_request(
        &runtime,
        rpc("tools/list", Some(json!(3202)), mcp_2026_params(json!({}))),
        None,
    )
    .await;
    let McpOutcome::Ok(plain_tools) = plain_tools else {
        panic!("expected ordinary tools/list");
    };
    for name in RESULT_APP_TOOLS {
        assert!(tool(&plain_tools["result"], name).get("_meta").is_none());
    }

    let resources = handle_mcp_request(
        &runtime,
        rpc(
            "resources/list",
            Some(json!(3203)),
            mcp_2026_ui_params(json!({})),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(resources) = resources else {
        panic!("expected UI resources/list");
    };
    let resources = resources["result"]["resources"].as_array().unwrap();
    assert!(resources
        .iter()
        .any(|resource| resource["uri"] == MCP_COMPUTER_UI_RESOURCE_URI));
    let result_resource = resources
        .iter()
        .find(|resource| resource["uri"] == MCP_RESULT_UI_RESOURCE_URI)
        .expect("Result App resource");
    assert_eq!(result_resource["mimeType"], MCP_UI_RESOURCE_MIME_TYPE);
    assert_eq!(
        result_resource["_meta"],
        json!({
            "ui": {
                "prefersBorder": true,
                "csp": {"connectDomains": [], "resourceDomains": []}
            }
        })
    );

    let read = handle_mcp_request(
        &runtime,
        rpc(
            "resources/read",
            Some(json!(3204)),
            mcp_2026_params(json!({"uri": MCP_RESULT_UI_RESOURCE_URI})),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(read) = read else {
        panic!("expected Result App resource read");
    };
    assert_eq!(
        read["result"]["contents"][0]["uri"],
        MCP_RESULT_UI_RESOURCE_URI
    );
    assert_eq!(
        read["result"]["contents"][0]["mimeType"],
        MCP_UI_RESOURCE_MIME_TYPE
    );
    assert_eq!(read["result"]["contents"][0]["text"], MCP_RESULT_APP_HTML);

    let no_ui_resources = handle_mcp_request(
        &runtime,
        rpc(
            "resources/list",
            Some(json!(3205)),
            mcp_2026_params(json!({})),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(no_ui_resources) = no_ui_resources else {
        panic!("expected non-UI resources/list");
    };
    assert!(no_ui_resources["result"]["resources"]
        .as_array()
        .unwrap()
        .is_empty());

    let local_runtime = test_runtime_with_surface(ModelSurface::LocalCoding);
    let local_tools = handle_mcp_request(
        &local_runtime,
        rpc(
            "tools/list",
            Some(json!(3206)),
            mcp_2026_ui_params(json!({})),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(local_tools) = local_tools else {
        panic!("expected LocalCoding tools/list");
    };
    assert!(local_tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|tool| tool
            .pointer("/_meta/ui/resourceUri")
            .and_then(Value::as_str)
            != Some(MCP_RESULT_UI_RESOURCE_URI)));

    let connector_tools =
        super::super::tools::project_connector_tools_list_payload_with_compact(false);
    assert!(connector_tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|tool| tool
            .pointer("/_meta/ui/resourceUri")
            .and_then(Value::as_str)
            != Some(MCP_RESULT_UI_RESOURCE_URI)));

    assert!(!mcp_app_enabled(
        true,
        true,
        ModelSurface::LocalCoding,
        &mcp_2026_ui_params(json!({}))
    ));
    assert!(!mcp_app_enabled(
        false,
        true,
        ModelSurface::FullOperatorRuntime,
        &mcp_2026_ui_params(json!({}))
    ));
}

#[tokio::test]
async fn server_mcp_apps_setting_disables_only_app_presentation() {
    let runtime = test_runtime_with_surface(ModelSurface::FullOperatorRuntime);

    let enabled = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/list",
            Some(json!(3207)),
            mcp_2026_ui_params(json!({})),
        ),
        None,
        true,
    )
    .await;
    let McpOutcome::Ok(enabled) = enabled else {
        panic!("enabled MCP Apps tools/list failed");
    };
    assert_eq!(
        tool(&enabled["result"], "list_jobs")["_meta"]["ui"]["resourceUri"],
        MCP_RESULT_UI_RESOURCE_URI
    );

    let discover = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "server/discover",
            Some(json!(3208)),
            mcp_2026_ui_params(json!({})),
        ),
        None,
        false,
    )
    .await;
    let McpOutcome::Ok(discover) = discover else {
        panic!("MCP discovery with Apps disabled failed");
    };
    let capabilities = &discover["result"]["capabilities"];
    assert_eq!(capabilities["resources"]["listChanged"], false);
    assert!(capabilities["extensions"].get(MCP_UI_EXTENSION).is_none());

    let tools = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/list",
            Some(json!(3209)),
            mcp_2026_ui_params(json!({})),
        ),
        None,
        false,
    )
    .await;
    let McpOutcome::Ok(tools) = tools else {
        panic!("tools/list with Apps disabled failed");
    };
    for name in RESULT_APP_TOOLS {
        assert!(tool(&tools["result"], name)
            .pointer("/_meta/ui/resourceUri")
            .is_none());
    }

    let resources = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "resources/list",
            Some(json!(3213)),
            mcp_2026_ui_params(json!({})),
        ),
        None,
        false,
    )
    .await;
    let McpOutcome::Ok(resources) = resources else {
        panic!("resources/list with Apps disabled failed");
    };
    assert!(resources["result"]["resources"]
        .as_array()
        .is_some_and(Vec::is_empty));

    let read = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "resources/read",
            Some(json!(3214)),
            mcp_2026_ui_params(json!({"uri": MCP_RESULT_UI_RESOURCE_URI})),
        ),
        None,
        false,
    )
    .await;
    match read {
        McpOutcome::BadRequest(value) => {
            assert_eq!(value["error"]["code"], -32602);
            assert!(value["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("disabled by Server configuration")));
        }
        other => panic!("disabled static App resource must fail closed: {other:?}"),
    }

    let computer_read = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "resources/read",
            Some(json!(3216)),
            mcp_2026_ui_params(json!({"uri": MCP_COMPUTER_UI_RESOURCE_URI})),
        ),
        None,
        false,
    )
    .await;
    match computer_read {
        McpOutcome::BadRequest(value) => {
            assert_eq!(value["error"]["code"], -32602);
            assert!(value["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("disabled by Server configuration")));
        }
        other => panic!("disabled Computer App resource must fail closed: {other:?}"),
    }

    let call = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(3215)),
            mcp_2026_ui_params(json!({
                "name": "list_jobs",
                "arguments": {"limit": 1}
            })),
        ),
        None,
        false,
    )
    .await;
    let McpOutcome::Ok(call) = call else {
        panic!("canonical list_jobs call with Apps disabled failed");
    };
    assert!(call["result"]["structuredContent"].is_object());
    assert!(call["result"]["_meta"]
        .get(super::super::presentation::MCP_PRESENTATION_META_KEY)
        .is_none());
}

#[test]
fn job_presentation_is_post_result_bounded_and_private() {
    let secret = "SECRET-SHOULD-NOT-REACH-PRESENTATION";
    let jobs = (0..12)
        .map(|index| {
            json!({
                "job_id": format!("job-{index}"),
                "status": if index == 0 { "lost" } else { "running" },
                "project": "p".repeat(400),
                "active": index != 0,
                "blocking_active": index != 0,
                "terminal": index == 0,
                "terminal_pending": false,
                "duration_ms": 25,
                "elapsed_secs": 1,
                "command_execution_state": if index == 0 { "outcome_unknown" } else { "completed" },
                "recovery_state": if index == 0 { "recovering" } else { "reconciled" },
                "recovery_reason": "r".repeat(400),
                "stdout_tail": secret,
                "stderr_tail": secret,
                "command_summary": secret,
                "cwd": format!("/private/{secret}"),
                "observation_token": secret,
            })
        })
        .collect::<Vec<_>>();
    let canonical = ToolResult::ok(json!({
        "jobs": jobs,
        "count": 12,
        "matched_count": 12,
        "truncated": true
    }));
    let mut framed = super::super::tools::mcp_runtime_tool_result("list_jobs", false, canonical);
    let structured_before = framed["structuredContent"].clone();
    super::super::presentation::attach_result_app_presentation("list_jobs", &mut framed);
    assert_eq!(framed["structuredContent"], structured_before);

    let meta = presentation(&framed);
    assert_eq!(meta["kind"], "job_list");
    assert_eq!(meta["items"].as_array().unwrap().len(), 8);
    assert_eq!(meta["items_truncated"], true);
    assert_eq!(meta["truncated"], true);
    assert_eq!(meta["items"][0]["status"], "lost");
    assert_eq!(
        meta["items"][0]["command_execution_state"],
        "outcome_unknown"
    );
    assert_eq!(meta["items"][0]["recovery_state"], "recovering");
    assert_eq!(meta["items"][0]["terminal"], true);
    assert_eq!(meta["items"][0]["active"], false);
    assert_eq!(meta["items"][0]["blocking_active"], false);
    assert_eq!(meta["items"][0]["terminal_pending"], false);
    assert!(meta["items"][0]["guidance"]
        .as_str()
        .unwrap()
        .contains("uncertain"));

    fn assert_bounded_strings(value: &Value) {
        match value {
            Value::String(text) => assert!(
                text.chars().count() <= super::super::presentation::MAX_MCP_PRESENTATION_TEXT_CHARS,
                "presentation text exceeded bound: {}",
                text.chars().count()
            ),
            Value::Array(values) => values.iter().for_each(assert_bounded_strings),
            Value::Object(values) => values.values().for_each(assert_bounded_strings),
            _ => {}
        }
    }
    assert_bounded_strings(meta);
    let serialized = serde_json::to_string(meta).unwrap();
    assert!(!serialized.contains(secret));
    for forbidden in [
        "stdout_tail",
        "stderr_tail",
        "command_summary",
        "observation_token",
        "cwd",
    ] {
        assert!(!serialized.contains(forbidden));
    }
}

#[test]
fn observe_presentation_preserves_wait_uncertainty_and_unknown_job_without_log_bodies() {
    let canonical = ToolResult::ok(json!({
        "requested_count": 2,
        "returned_count": 2,
        "succeeded_count": 1,
        "failed_count": 1,
        "items": [
            {
                "job_id": "job-uncertain",
                "success": true,
                "output": {
                    "job_id": "job-uncertain",
                    "status": "lost",
                    "terminal": true,
                    "changed": true,
                    "command_execution_state": "outcome_unknown",
                    "recovery_state": "lost_after_reconcile",
                    "log_delta_status": "delta",
                    "stdout_lines": 500,
                    "stderr_lines": 2,
                    "stdout_tail": "SECRET-STDOUT",
                    "stderr_tail": "SECRET-STDERR",
                    "observation_token": "opaque-secret-token"
                }
            },
            {
                "job_id": "missing-job",
                "success": false,
                "error_kind": "unknown_job",
                "recovery_kind": "reobserve",
                "recovery_tool": "list_jobs",
                "error": "unbounded internal error text"
            }
        ],
        "wait": {"outcome": "item_error", "waited_ms": 7},
        "changed_count": 1,
        "terminal_count": 1,
        "output_truncated": false,
        "next_index": null
    }));
    let mut framed = super::super::tools::mcp_runtime_tool_result("observe_jobs", false, canonical);
    let structured_before = framed["structuredContent"].clone();
    super::super::presentation::attach_result_app_presentation("observe_jobs", &mut framed);
    assert_eq!(framed["structuredContent"], structured_before);
    let meta = presentation(&framed);
    assert_eq!(meta["wait"]["outcome"], "item_error");
    assert_eq!(meta["items"][0]["status"], "lost");
    assert_eq!(
        meta["items"][0]["command_execution_state"],
        "outcome_unknown"
    );
    assert_eq!(meta["items"][1]["error_kind"], "unknown_job");
    assert_eq!(meta["items"][1]["recovery_tool"], "list_jobs");
    let serialized = serde_json::to_string(meta).unwrap();
    for forbidden in [
        "SECRET-STDOUT",
        "SECRET-STDERR",
        "opaque-secret-token",
        "unbounded internal error text",
        "stdout_tail",
        "stderr_tail",
        "observation_token",
    ] {
        assert!(!serialized.contains(forbidden));
    }
}

#[test]
fn validation_run_presentation_preserves_canonical_state_matrix() {
    let cases = [
        (
            "completed_passed",
            true,
            json!({
                "execution_state": "completed", "terminal": true, "passed": true,
                "command_started": true, "command_completed": true,
                "promoted_to_job": false, "duration_ms": 1840, "exit_code": 0,
                "tests_detected": true, "tests_run_count": 42,
                "tests_passed": 42, "tests_failed": 0, "zero_tests_run": false
            }),
            "completed",
            Some(true),
            None,
        ),
        (
            "validation_failed",
            false,
            json!({
                "execution_state": "completed", "terminal": true, "passed": false,
                "failure_kind": "validation_failed", "command_started": true,
                "command_completed": true, "promoted_to_job": false, "exit_code": 101
            }),
            "completed",
            Some(false),
            Some("validation_failed"),
        ),
        (
            "process_exit",
            false,
            json!({
                "execution_state": "completed", "terminal": true, "passed": false,
                "failure_kind": "process_exit", "command_started": true,
                "command_completed": true, "promoted_to_job": false, "exit_code": 2
            }),
            "completed",
            Some(false),
            Some("process_exit"),
        ),
        (
            "promoted_running",
            true,
            json!({
                "execution_state": "running", "terminal": false,
                "command_started": true, "command_completed": false,
                "promoted_to_job": true, "job_id": "job-validation",
                "job_status": "running", "observation_token": "canonical-only-token"
            }),
            "running",
            None,
            None,
        ),
        (
            "outcome_unknown",
            false,
            json!({
                "execution_state": "outcome_unknown", "terminal": false, "passed": false,
                "failure_kind": "outcome_unknown", "command_started": true,
                "command_completed": false, "promoted_to_job": false
            }),
            "outcome_unknown",
            Some(false),
            Some("outcome_unknown"),
        ),
        (
            "timed_out",
            false,
            json!({
                "execution_state": "timed_out", "terminal": true, "passed": false,
                "failure_kind": "timeout", "command_started": true,
                "command_completed": false, "promoted_to_job": false
            }),
            "timed_out",
            Some(false),
            Some("timeout"),
        ),
        (
            "pre_start",
            false,
            json!({
                "execution_state": "not_started", "terminal": true, "passed": false,
                "failure_kind": "permission_denied", "command_started": false,
                "command_completed": false, "promoted_to_job": false
            }),
            "not_started",
            Some(false),
            Some("permission_denied"),
        ),
    ];

    for (label, success, output, execution_state, passed, failure_kind) in cases {
        let framed = projected_result("cargo_test", success, output);
        let meta = presentation(&framed);
        assert_eq!(meta["kind"], "validation_run", "{label}");
        assert_eq!(meta["tool"], "cargo_test", "{label}");
        assert_eq!(meta["validation_kind"], "test", "{label}");
        assert_eq!(meta["execution_state"], execution_state, "{label}");
        match passed {
            Some(value) => assert_eq!(meta["passed"], value, "{label}"),
            None => assert!(meta.get("passed").is_none(), "{label}"),
        }
        match failure_kind {
            Some(value) => assert_eq!(meta["failure_kind"], value, "{label}"),
            None => assert!(meta.get("failure_kind").is_none(), "{label}"),
        }
    }
}

#[test]
fn validation_run_presentation_bounds_diagnostics_and_excludes_private_canonical_fields() {
    let secret = "VALIDATION-PRIVATE-SECRET";
    let diagnostics = (0..4)
        .map(|index| {
            json!({
                "severity": "error",
                "code": "C".repeat(300),
                "message": format!("{index}-{}", "😀".repeat(300)),
                "file": format!("/private/{secret}/{index}.rs"),
                "line": 123,
                "column": 7
            })
        })
        .collect::<Vec<_>>();
    let failed_tests = (0..9)
        .map(|index| {
            json!({
                "name": format!("test_{}", "名".repeat(300)),
                "failure_kind": "assertion",
                "file": format!("/private/{secret}/test-{index}.rs"),
                "line": 9,
                "column": 2
            })
        })
        .collect::<Vec<_>>();
    let output = json!({
        "execution_state": "completed",
        "terminal": true,
        "passed": false,
        "failure_kind": "validation_failed",
        "command_started": true,
        "command_completed": true,
        "promoted_to_job": false,
        "stdout_tail": secret,
        "stderr_tail": secret,
        "stdout_evidence": secret,
        "stderr_evidence": secret,
        "command_summary": format!("cargo test -- {secret}"),
        "command": secret,
        "argv": [secret],
        "cwd": format!("/private/{secret}"),
        "affected_paths": [format!("/private/{secret}/src")],
        "credential": secret,
        "Authorization": secret,
        "token": secret,
        "observation_token": secret,
        "identity": secret,
        "detected_summary": {"raw": secret},
        "diagnostics": {
            "available": true,
            "diagnostic_count": 13,
            "returned_diagnostic_count": 13,
            "diagnostics_truncated": false,
            "failed_test_details_truncated": false,
            "diagnostics": diagnostics,
            "failed_test_details": failed_tests,
            "raw_parser_output": secret
        }
    });
    let framed = projected_result("cargo_test", false, output);
    let meta = presentation(&framed);
    let safe_diagnostics = &meta["diagnostics"];
    assert_eq!(safe_diagnostics["items"].as_array().unwrap().len(), 4);
    assert_eq!(
        safe_diagnostics["failed_tests"].as_array().unwrap().len(),
        4
    );
    assert_eq!(safe_diagnostics["presentation_items_truncated"], true);
    assert!(safe_diagnostics["items"][0].get("file").is_none());
    assert!(safe_diagnostics["failed_tests"][0].get("file").is_none());
    assert_eq!(
        safe_diagnostics["items"][0]["message"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        super::super::presentation::MAX_MCP_PRESENTATION_TEXT_CHARS
    );
    assert_presentation_strings_bounded(meta);
    let serialized = serde_json::to_string(meta).unwrap();
    for forbidden in [
        secret,
        "stdout_tail",
        "stderr_tail",
        "stdout_evidence",
        "stderr_evidence",
        "command_summary",
        "command\"",
        "argv",
        "cwd",
        "affected_paths",
        "credential",
        "Authorization",
        "token",
        "observation_token",
        "identity",
        "raw_parser_output",
        "detected_summary",
        "file",
    ] {
        assert!(!serialized.contains(forbidden), "leaked {forbidden}");
    }
}

#[test]
fn validation_summary_presentation_preserves_evidence_statuses_and_history_boundaries() {
    let cases = [
        ("passed", "passed", 0),
        ("failed", "failed", 0),
        ("mixed", "passed", 0),
        ("inconclusive", "inconclusive", 0),
        ("expected", "expected", 0),
        ("passed", "stale", 0),
        ("not_run", "not_run", 0),
        ("unknown", "unknown", 1),
    ];
    for (status, current_status, evidence_gap_count) in cases {
        let framed = projected_result(
            "validation_summary",
            true,
            json!({
                "validation": {
                    "available": status != "not_run",
                    "status": status,
                    "latest_status": if status == "mixed" { "passed" } else { status },
                    "reason": null,
                    "current_evidence": {
                        "status": current_status,
                        "reason": if current_status == "stale" { Some("workspace changed") } else { None },
                        "latest_status": if current_status == "stale" { "passed" } else { current_status },
                        "events_total": 2,
                        "successes": 1,
                        "failures": 1,
                        "expected_results": 0,
                        "resolved_failure_count": 1,
                        "unresolved_failure_count": 0,
                        "evidence_gap_event_count": evidence_gap_count,
                        "stale_failure_count": if current_status == "stale" { 1 } else { 0 },
                        "evidence_after_latest_content_change": current_status != "stale",
                        "boundary_reason": "workspace_content_changed"
                    },
                    "historical_failures": {"count": 3, "resolved": true, "unresolved": false},
                    "resolved_failures": {"count": 3, "events": []},
                    "unresolved_failures": {"count": 0, "events": []},
                    "evidence_gaps": {"count": evidence_gap_count, "events": []},
                    "cargo_test_zero_tests_run": false,
                    "events": []
                }
            }),
        );
        let meta = presentation(&framed);
        assert_eq!(meta["kind"], "validation_summary");
        assert_eq!(meta["validation"]["status"], status);
        assert_eq!(
            meta["validation"]["current_evidence"]["status"],
            current_status
        );
        assert_eq!(meta["validation"]["historical_failures"]["count"], 3);
        assert_eq!(meta["validation"]["resolved_failures"]["count"], 3);
        assert_eq!(meta["validation"]["unresolved_failures"]["count"], 0);
        assert_eq!(
            meta["validation"]["evidence_gaps"]["count"],
            evidence_gap_count
        );
    }
}

#[test]
fn validation_summary_presentation_bounds_events_and_excludes_private_event_fields() {
    let secret = "LEDGER-PRIVATE-SECRET";
    let events = (0..12)
        .map(|index| {
            json!({
                "tool_name": if index % 2 == 0 { "cargo_check" } else { "cargo_test" },
                "validation_kind": if index % 2 == 0 { "check" } else { "test" },
                "success": index % 3 != 0,
                "execution_success": index % 3 != 0,
                "validation_passed": index % 4 != 0,
                "expectation_satisfied": index % 5 == 0,
                "failure_class": "execution_or_correctness",
                "failure_kind": "validation_failed",
                "unresolved_failure": index == 0,
                "summary": format!("{index}-{}", "证".repeat(300)),
                "duration_ms": 33,
                "tests_run_count": 7,
                "diagnostics": {"test_summary": {"passed": 6, "failed": 1}},
                "command_summary": secret,
                "cwd": format!("/private/{secret}"),
                "affected_paths": [secret],
                "identity": secret,
                "stdout_evidence": secret,
                "stderr_evidence": secret,
                "raw_parser_output": secret
            })
        })
        .collect::<Vec<_>>();
    let framed = projected_result(
        "validation_summary",
        true,
        json!({
            "validation": {
                "available": true,
                "status": "mixed",
                "latest_status": "passed",
                "current_evidence": {
                    "status": "passed", "reason": null, "latest_status": "passed",
                    "events_total": 1, "successes": 1, "failures": 0, "expected_results": 0,
                    "resolved_failure_count": 0, "unresolved_failure_count": 0,
                    "evidence_gap_event_count": 0, "stale_failure_count": 0,
                    "evidence_after_latest_content_change": true,
                    "boundary_reason": "attempt_start"
                },
                "historical_failures": {"count": 4, "resolved": true, "unresolved": false},
                "resolved_failures": {"count": 4, "events": []},
                "unresolved_failures": {"count": 0, "events": []},
                "evidence_gaps": {"count": 0, "events": []},
                "cargo_test_zero_tests_run": false,
                "events": events
            }
        }),
    );
    let meta = presentation(&framed);
    assert_eq!(meta["validation"]["events"].as_array().unwrap().len(), 8);
    assert_eq!(meta["validation"]["events_truncated"], true);
    assert_eq!(meta["validation"]["events"][0]["tests_passed"], 6);
    assert_eq!(meta["validation"]["events"][0]["tests_failed"], 1);
    assert_eq!(meta["validation"]["events"][0]["success"], true);
    assert_eq!(meta["validation"]["events"][0]["validation_passed"], false);
    assert!(meta["validation"]["events"][0]["summary"]
        .as_str()
        .unwrap()
        .starts_with("4-"));
    assert_presentation_strings_bounded(meta);
    let serialized = serde_json::to_string(meta).unwrap();
    for forbidden in [
        secret,
        "command_summary",
        "cwd",
        "affected_paths",
        "identity",
        "stdout_evidence",
        "stderr_evidence",
        "raw_parser_output",
    ] {
        assert!(!serialized.contains(forbidden), "leaked {forbidden}");
    }
}

#[test]
fn validation_presentation_is_fail_open_for_unknown_shapes_and_unbound_tools() {
    let canonical = ToolResult::ok(json!({"future_validation_shape": [1, 2, 3]}));
    let mut framed = super::super::tools::mcp_runtime_tool_result("cargo_check", false, canonical);
    let structured_before = framed["structuredContent"].clone();
    super::super::presentation::attach_result_app_presentation("cargo_check", &mut framed);
    assert_eq!(framed["structuredContent"], structured_before);
    assert_eq!(presentation(&framed)["kind"], "validation_run");

    let canonical = ToolResult::ok(json!({"execution_state": "completed", "passed": true}));
    let mut unbound = super::super::tools::mcp_runtime_tool_result("cargo_fmt", false, canonical);
    let structured_before = unbound["structuredContent"].clone();
    super::super::presentation::attach_result_app_presentation("cargo_fmt", &mut unbound);
    assert_eq!(unbound["structuredContent"], structured_before);
    assert!(unbound
        .get("_meta")
        .and_then(|meta| meta.get(super::super::presentation::MCP_PRESENTATION_META_KEY))
        .is_none());
}

#[test]
fn result_app_html_is_display_only_and_uses_safe_dom_rendering() {
    let html = MCP_RESULT_APP_HTML;
    for expected in [
        "ui/initialize",
        "ui/notifications/initialized",
        "ui/notifications/tool-result",
        "webcodex/presentation",
        "textContent",
        "document.createElement",
        "document.createElement(\"details\")",
        "validation_run",
        "validation_summary",
        "Cargo Test",
        "Array.from",
        "Cargo test zero tests",
    ] {
        assert!(html.contains(expected), "missing {expected}");
    }
    for forbidden in [
        "innerHTML",
        "eval(",
        "new Function",
        "localStorage",
        "indexedDB",
        "fetch(",
        "WebSocket",
        "tools/call",
        "callServerTool",
        "ui/update-model-context",
        "ui/message",
        "<button",
    ] {
        assert!(
            !html.contains(forbidden),
            "Result App must not contain {forbidden}"
        );
    }
}

fn result_app_auth() -> crate::auth::AuthContext {
    crate::auth::AuthContext {
        kind: crate::auth::AuthKind::Bootstrap,
        user_id: Some("user-result-app-owner".to_string()),
        username: Some("result-app-owner".to_string()),
        api_key_id: Some("key-result-app-owner".to_string()),
        role: Some("admin".to_string()),
        scopes: vec!["admin".to_string()],
        is_bootstrap: true,
        token_kind: None,
        allowed_client_id: None,
        shared_key_hash: None,
        project_grant_id: None,
    }
}

async fn register_job_runner(runtime: &ToolRuntime, auth: &crate::auth::AuthContext) {
    let capabilities = RunnerCapabilities {
        async_jobs: true,
        async_shell_jobs: true,
        structured_validation_argv: true,
        ..Default::default()
    };
    runtime
        .runner_registry
        .register_with_auth(
            crate::test_support::current_runner_registration(RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: "result-app-runner".to_string(),
                runner_instance_id: "inst-result-app".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                host_context: None,
                capabilities: crate::test_support::current_runner_capabilities(capabilities),
                policy: None,
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            }),
            Some(&crate::test_support::runner_access(auth)),
        )
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        "result-app-runner",
        "inst-result-app",
        vec![RunnerProjectSummary {
            id: "demo".to_string(),
            name: Some("Result App Demo".to_string()),
            path: "/tmp/result-app-demo".to_string(),
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
}

async fn set_job_state(
    runtime: &ToolRuntime,
    request: &crate::runner_protocol::RunnerRequest,
    status: &str,
    finished: bool,
) {
    runtime
        .runner_registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "result-app-runner".to_string(),
            runner_instance_id: "inst-result-app".to_string(),
            update_seq: None,
            job_id: request.job_id.clone().expect("Job id"),
            request_id: Some(request.request_id.clone()),
            status: status.to_string(),
            stdout_chunk: Some("secret log body that presentation must not copy\n".to_string()),
            stderr_chunk: None,
            stdout_tail: None,
            stderr_tail: None,
            log_snapshot: None,
            exit_code: finished.then_some(0),
            duration_ms: finished.then_some(25),
            error: None,
            command_execution_state: finished
                .then_some(crate::runner_protocol::ShellCommandExecutionState::Completed),
            validation_progress: None,
            activity: None,
            finished,
        })
        .await
        .unwrap();
}

async fn wait_for_result_app_runner_request(
    runtime: &ToolRuntime,
) -> crate::runner_protocol::RunnerRequest {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(request) = runtime
                .runner_registry
                .poll(RunnerPollRequest {
                    client_id: "result-app-runner".to_string(),
                    runner_instance_id: "inst-result-app".to_string(),
                })
                .await
                .unwrap()
            {
                return request;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("timed out waiting for Result App Runner request")
}

async fn complete_result_app_validation_job(
    runtime: &ToolRuntime,
    request: &crate::runner_protocol::RunnerRequest,
) {
    runtime
        .runner_registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "result-app-runner".to_string(),
            runner_instance_id: "inst-result-app".to_string(),
            update_seq: None,
            job_id: request.job_id.clone().expect("validation Job id"),
            request_id: Some(request.request_id.clone()),
            status: "completed".to_string(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: Some("Finished `dev` profile [unoptimized] target(s)\n".to_string()),
            stderr_tail: Some(String::new()),
            log_snapshot: None,
            exit_code: Some(0),
            duration_ms: Some(25),
            error: None,
            command_execution_state: None,
            validation_progress: Some(crate::runner_protocol::ShellJobValidationProgress {
                completed: 1,
                current_step: None,
                failed_step: None,
            }),
            activity: None,
            finished: true,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn mcp_job_presentation_tracks_real_running_to_terminal_transition() {
    let runtime = test_runtime_with_surface(ModelSurface::FullOperatorRuntime);
    let auth = result_app_auth();
    register_job_runner(&runtime, &auth).await;
    let started = runtime
        .dispatch_with_auth(
            ToolCall::RunJob {
                project: "agent:result-app-runner:demo".to_string(),
                command: "echo result-app".to_string(),
                session_id: None,
                timeout_secs: Some(60),
                cwd: None,
                purpose: None,
                shell: None,
            },
            Some(&auth),
        )
        .await;
    assert!(started.success, "{:?}", started.error);
    let job_id = started.output["job_id"].as_str().unwrap().to_string();
    let request = runtime
        .runner_registry
        .poll(RunnerPollRequest {
            client_id: "result-app-runner".to_string(),
            runner_instance_id: "inst-result-app".to_string(),
        })
        .await
        .unwrap()
        .expect("queued Job request");
    set_job_state(&runtime, &request, "running", false).await;

    async fn observe(
        runtime: &ToolRuntime,
        auth: &crate::auth::AuthContext,
        job_id: &str,
        id: i64,
    ) -> Value {
        let outcome = handle_mcp_request(
            runtime,
            rpc(
                "tools/call",
                Some(json!(id)),
                mcp_2026_ui_params(json!({
                    "name": "observe_jobs",
                    "arguments": {"items": [{"job_id": job_id}]}
                })),
            ),
            Some(auth),
        )
        .await;
        let McpOutcome::Ok(body) = outcome else {
            panic!("expected observe_jobs MCP result");
        };
        body["result"].clone()
    }

    let running = observe(&runtime, &auth, &job_id, 3210).await;
    assert_eq!(presentation(&running)["items"][0]["status"], "running");
    assert_eq!(presentation(&running)["items"][0]["terminal"], false);
    assert!(serde_json::to_string(presentation(&running))
        .unwrap()
        .find("secret log body")
        .is_none());

    let listed = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(32101)),
            mcp_2026_ui_params(json!({
                "name": "list_jobs",
                "arguments": {"project": "agent:result-app-runner:demo", "limit": 10}
            })),
        ),
        Some(&auth),
    )
    .await;
    let McpOutcome::Ok(listed) = listed else {
        panic!("expected list_jobs MCP result");
    };
    assert_eq!(presentation(&listed["result"])["kind"], "job_list");
    assert!(presentation(&listed["result"])["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["job_id"] == job_id && item["status"] == "running"));

    let plain_listed = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(32102)),
            mcp_2026_params(json!({
                "name": "list_jobs",
                "arguments": {"project": "agent:result-app-runner:demo", "limit": 10}
            })),
        ),
        Some(&auth),
    )
    .await;
    let McpOutcome::Ok(plain_listed) = plain_listed else {
        panic!("expected non-UI list_jobs MCP result");
    };
    assert!(plain_listed["result"]["_meta"]
        .get(super::super::presentation::MCP_PRESENTATION_META_KEY)
        .is_none());
    assert_eq!(
        plain_listed["result"]["structuredContent"]["output"]["jobs"][0]["job_id"],
        job_id
    );

    set_job_state(&runtime, &request, "completed", true).await;
    let completed = observe(&runtime, &auth, &job_id, 3211).await;
    assert_eq!(presentation(&completed)["items"][0]["status"], "completed");
    assert_eq!(presentation(&completed)["items"][0]["terminal"], true);
    assert_eq!(
        completed["structuredContent"]["output"]["items"][0]["status"],
        "completed"
    );

    let unknown = observe(&runtime, &auth, "unknown-result-app-job", 3212).await;
    assert_eq!(
        presentation(&unknown)["items"][0]["error_kind"],
        "unknown_job"
    );
    assert_eq!(
        presentation(&unknown)["items"][0]["recovery_tool"],
        "list_jobs"
    );

    assert!(runtime.runner_registry.remove_job_record(&job_id).await);
}

#[tokio::test]
async fn mcp_validation_run_and_summary_use_real_canonical_contracts() {
    std::fs::create_dir_all("/tmp/result-app-demo").unwrap();
    let runtime = test_runtime_with_surface(ModelSurface::FullOperatorRuntime)
        .with_validation_sync_wait(std::time::Duration::from_millis(500));
    let auth = result_app_auth();
    register_job_runner(&runtime, &auth).await;
    let project = "agent:result-app-runner:demo";
    let session = runtime.sessions.start_session(
        Some(project.to_string()),
        Some("result app validation".to_string()),
    );

    let cargo_call = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let session_id = session.session_id.clone();
        async move {
            handle_mcp_request(
                &runtime,
                rpc(
                    "tools/call",
                    Some(json!(3220)),
                    mcp_2026_ui_params(json!({
                        "name": "cargo_check",
                        "arguments": {
                            "project": project,
                            "session_id": session_id,
                            "timeout_secs": 60,
                            "sync_wait_secs": 1
                        }
                    })),
                ),
                Some(&auth),
            )
            .await
        }
    });
    let request = wait_for_result_app_runner_request(&runtime).await;
    assert_eq!(request.kind, "start_validation_job");
    complete_result_app_validation_job(&runtime, &request).await;
    let McpOutcome::Ok(cargo_call) = cargo_call.await.unwrap() else {
        panic!("expected real cargo_check MCP result");
    };
    let cargo_result = &cargo_call["result"];
    assert_eq!(
        cargo_result["structuredContent"]["output"]["execution_state"],
        "completed"
    );
    assert_eq!(cargo_result["structuredContent"]["output"]["passed"], true);
    assert_eq!(presentation(cargo_result)["kind"], "validation_run");
    assert_eq!(presentation(cargo_result)["tool"], "cargo_check");
    assert_eq!(presentation(cargo_result)["execution_state"], "completed");
    assert_eq!(presentation(cargo_result)["passed"], true);
    assert!(!serde_json::to_string(presentation(cargo_result))
        .unwrap()
        .contains("secret log body"));

    let summary = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(3221)),
            mcp_2026_ui_params(json!({
                "name": "validation_summary",
                "arguments": {"project": project, "session_id": session.session_id}
            })),
        ),
        Some(&auth),
    )
    .await;
    let McpOutcome::Ok(summary) = summary else {
        panic!("expected real validation_summary MCP result");
    };
    let summary_result = &summary["result"];
    let canonical_validation = &summary_result["structuredContent"]["output"]["validation"];
    assert_eq!(canonical_validation["status"], "passed");
    assert_eq!(canonical_validation["current_evidence"]["status"], "passed");
    assert_eq!(presentation(summary_result)["kind"], "validation_summary");
    assert_eq!(
        presentation(summary_result)["validation"]["status"],
        canonical_validation["status"]
    );
    assert_eq!(
        presentation(summary_result)["validation"]["current_evidence"]["status"],
        canonical_validation["current_evidence"]["status"]
    );
    assert_eq!(
        presentation(summary_result)["validation"]["events"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let disabled = handle_with_server_apps_enabled(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(3222)),
            mcp_2026_ui_params(json!({
                "name": "validation_summary",
                "arguments": {"project": project, "session_id": session.session_id}
            })),
        ),
        Some(&auth),
        false,
    )
    .await;
    let McpOutcome::Ok(disabled) = disabled else {
        panic!("Apps-disabled validation_summary should remain callable");
    };
    assert_eq!(
        disabled["result"]["structuredContent"],
        summary_result["structuredContent"]
    );
    assert!(disabled["result"]["_meta"]
        .get(super::super::presentation::MCP_PRESENTATION_META_KEY)
        .is_none());

    let plain = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(3223)),
            mcp_2026_params(json!({
                "name": "validation_summary",
                "arguments": {"project": project, "session_id": session.session_id}
            })),
        ),
        Some(&auth),
    )
    .await;
    let McpOutcome::Ok(plain) = plain else {
        panic!("non-UI validation_summary should remain callable");
    };
    assert_eq!(
        plain["result"]["structuredContent"],
        summary_result["structuredContent"]
    );
    assert!(plain["result"]["_meta"]
        .get(super::super::presentation::MCP_PRESENTATION_META_KEY)
        .is_none());
}

#[test]
fn observation_token_remains_only_in_canonical_result() {
    let canonical = ToolResult::ok(json!({
        "items": [{
            "job_id": "job-token",
            "status": "running",
            "terminal": false,
            "changed": false,
            "log_delta_status": "unchanged",
            "observation_token": "opaque-continuation-token"
        }],
        "wait": {"outcome": "immediate"}
    }));
    let mut framed = super::super::tools::mcp_runtime_tool_result("observe_jobs", false, canonical);
    super::super::presentation::attach_result_app_presentation("observe_jobs", &mut framed);
    assert_eq!(
        framed["structuredContent"]["output"]["items"][0]["observation_token"],
        "opaque-continuation-token"
    );
    assert!(!serde_json::to_string(presentation(&framed))
        .unwrap()
        .contains("opaque-continuation-token"));
}

#[test]
fn observe_jobs_item_limit_matches_presentation_bound() {
    assert_eq!(super::super::presentation::MAX_MCP_PRESENTATION_ITEMS, 8);
    let parsed = ToolCall::ObserveJobs {
        items: (0..8)
            .map(|index| ObserveJobsItem {
                job_id: format!("job-{index}"),
                after_observation_token: None,
            })
            .collect(),
        tail_lines: 40,
        wait_secs: None,
    };
    assert!(matches!(parsed, ToolCall::ObserveJobs { .. }));
}
