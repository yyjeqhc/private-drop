use crate::{
    current_validation_evidence_for_session, parse_cargo_test_run_metadata,
    validation_kind_for_tool, validation_summary_for_session_events,
};
use serde_json::{json, Value};
use webcodex_core::validation_evidence::{
    NO_STABLE_DIAGNOSTICS_REASON, PARSER_KIND, PARSER_VERSION,
    VALIDATION_OUTPUT_METADATA_ABSENT_REASON,
};
use webcodex_core::{
    runner_protocol::JOB_INVENTORY_MAX_TERMINAL_JOBS, workflow_session_contract::is_safe_job_id,
};
use webcodex_tool_contracts::{
    runtime_tool_accepts_context_ack, runtime_tool_advances_context_checkpoint,
    runtime_tool_is_change_summary_like, runtime_tool_is_git_like, runtime_tool_is_read_like,
    runtime_tool_is_shell_like, runtime_tool_is_write_like, runtime_tool_metadata,
    runtime_tool_session_risk_class, ToolPathHint, ToolRisk,
};
use webcodex_tool_runtime_contracts::{
    tool_audit::{
        assertion_validation_identity, session_log_arguments_for_tool_request,
        structured_validation_target_identity,
    },
    SessionMode,
};
use webcodex_workflow_session as sessions;
use webcodex_workflow_session::root_test_support::MAX_VALIDATION_EXCERPT_CHARS;
use webcodex_workflow_session::{
    SessionGuards, SessionPathHint, SessionStore, SessionToolContract, SessionTransport,
};

fn validation_summary_for_session(summary: &sessions::SessionSummary) -> Value {
    validation_summary_for_session_events(summary, &summary.events, 10)
}

fn session_tool_contract(tool_name: &str) -> SessionToolContract {
    let metadata = runtime_tool_metadata(tool_name);
    SessionToolContract {
        risk_class: runtime_tool_session_risk_class(tool_name),
        read_like: runtime_tool_is_read_like(tool_name),
        write_like: runtime_tool_is_write_like(tool_name),
        shell_like: runtime_tool_is_shell_like(tool_name),
        git_like: runtime_tool_is_git_like(tool_name),
        change_summary_like: runtime_tool_is_change_summary_like(tool_name),
        project_write: metadata.risk == ToolRisk::ProjectWrite,
        path_hint: match metadata.path_hint {
            ToolPathHint::None => SessionPathHint::None,
            ToolPathHint::SinglePath => SessionPathHint::SinglePath,
            ToolPathHint::PathList => SessionPathHint::PathList,
            ToolPathHint::Patch => SessionPathHint::Patch,
            ToolPathHint::Artifact => SessionPathHint::Artifact,
        },
        accepts_context_ack: runtime_tool_accepts_context_ack(tool_name),
        advances_context_checkpoint: runtime_tool_advances_context_checkpoint(tool_name),
    }
}

#[test]
fn validation_like_tool_calls_are_classified_correctly() {
    for (tool_name, validation_kind) in [
        ("cargo_fmt", "format"),
        ("cargo_check", "check"),
        ("cargo_test", "test"),
    ] {
        assert_eq!(validation_kind_for_tool(tool_name), Some(validation_kind));
    }

    assert_eq!(validation_kind_for_tool("run_shell"), None);
    assert_eq!(validation_kind_for_tool("apply_unified_diff"), None);
    assert_eq!(validation_kind_for_tool("read_file"), None);
}

#[test]
fn validation_summary_is_unavailable_without_validation_events() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "read_file",
        json!({"project": "agent:eval:demo", "path": "src/lib.rs"}),
        true,
        json!({}),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);

    assert_eq!(validation["available"], false);
    assert_eq!(validation["status"], "not_run");
    assert_eq!(validation["reason"], "no_validation_tool_invoked");
    assert_eq!(validation["source"], "session_ledger");
    assert_eq!(validation["events_total"], 0);
    assert!(validation["events"].as_array().unwrap().is_empty());
    assert_eq!(validation["parser"]["available"], false);
    assert_eq!(validation["parser"]["kind"], PARSER_KIND);
    assert_eq!(validation["parser"]["version"], PARSER_VERSION);
    assert_eq!(
        validation["parser"]["source"],
        "bounded_validation_metadata"
    );
    assert_eq!(validation["parser"]["raw_output_exposed"], false);
    assert_eq!(
        validation["parser"]["limitations"],
        json!([
            "bounded validation excerpts only",
            "deterministic evidence extraction; no root-cause inference",
            "no full stdout/stderr bodies; incomplete excerpts may omit fields or report unknown"
        ])
    );
    assert_eq!(
        validation["parser"]["reason"],
        VALIDATION_OUTPUT_METADATA_ABSENT_REASON
    );
    assert!(validation.get("latest_success").is_none());
    assert!(validation.get("latest_failure").is_none());
    assert!(validation.get("latest").is_some());
    assert!(validation["latest"].is_null());
    assert_eq!(validation["latest_status"], "not_run");
    assert_eq!(validation["historical_failures"]["count"], 0);
    assert_eq!(validation["historical_failures"]["resolved"], false);
    assert_eq!(validation["historical_failures"]["unresolved"], false);
}

#[test]
fn cargo_check_success_produces_validation_event() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_check",
        json!({
            "project": "agent:eval:demo",
            "cwd": null,
            "all_targets": true,
            "timeout_secs": 60,
        }),
        true,
        json!({"exit_code": 0}),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);
    let event = &validation["latest_success"];

    assert_eq!(validation["available"], true);
    assert_eq!(validation["status"], "passed");
    assert!(validation["reason"].is_null());
    assert_eq!(validation["events_total"], 1);
    assert_eq!(validation["successes"], 1);
    assert_eq!(validation["failures"], 0);
    assert_eq!(event["tool_name"], "cargo_check");
    assert_eq!(event["validation_kind"], "check");
    assert_eq!(event["success"], true);
    assert_eq!(event["exit_code"], 0);
    assert_eq!(event["summary"], "cargo_check succeeded");
    assert!(event.get("input_summary").is_none());
    assert!(event["identity"]
        .as_str()
        .is_some_and(|identity| identity.starts_with("target:")));
    assert_eq!(event["execution_source"], "cargo_check");
    assert_eq!(event["purpose"], "validation");
    assert_eq!(validation["parser"]["available"], false);
    assert_eq!(
        validation["parser"]["reason"],
        VALIDATION_OUTPUT_METADATA_ABSENT_REASON
    );
    assert!(event.get("diagnostics").is_none());
}

#[test]
fn validation_output_metadata_without_stable_diagnostics_makes_parser_available() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_check",
        json!({"project": "agent:eval:demo"}),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);
    let diagnostics = &validation["latest_success"]["diagnostics"];

    assert_eq!(validation["available"], true);
    assert_eq!(validation["events_total"], 1);
    assert_eq!(validation["latest_success"]["tool_name"], "cargo_check");
    assert_eq!(validation["parser"]["available"], true);
    assert_eq!(validation["status"], "passed");
    assert!(validation["reason"].is_null());
    assert_eq!(validation["parser"]["kind"], PARSER_KIND);
    assert_eq!(validation["parser"]["version"], 3);
    assert_eq!(
        validation["parser"]["source"],
        "bounded_validation_metadata"
    );
    assert_eq!(validation["parser"]["raw_output_exposed"], false);
    assert!(validation["parser"].get("reason").is_none());
    assert_eq!(diagnostics["available"], false);
    assert_eq!(diagnostics["parser"], PARSER_KIND);
    assert_eq!(diagnostics["reason"], NO_STABLE_DIAGNOSTICS_REASON);
    assert_no_raw_validation_output_fields(&validation, "validation summary");
}

#[test]
fn cargo_check_finished_event_records_safe_validation_output_summary() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    let stderr_tail = format!(
        "{}\nAuthorization: Bearer supersecret\nerror[E0308]: mismatched types\n --> src/lib.rs:12:5\n",
        "x".repeat(MAX_VALIDATION_EXCERPT_CHARS + 200)
    );
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_check",
        json!({"project": "agent:eval:demo"}),
        false,
        json!({
            "exit_code": 101,
            "stdout": "full stdout body must not be ledgered",
            "stderr": "full stderr body must not be ledgered",
            "stdout_tail": "api_key=supersecret\nsafe stdout line\n",
            "stderr_tail": stderr_tail,
            "stdout_truncated": false,
            "stderr_truncated": true,
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let finished = session
        .events
        .iter()
        .find(|event| event.kind == "tool_call_finished")
        .unwrap();
    let output_summary = finished.validation_output_summary.as_ref().unwrap();
    let stdout_excerpt = output_summary["stdout_tail_excerpt"].as_str().unwrap();
    let stderr_excerpt = output_summary["stderr_tail_excerpt"].as_str().unwrap();

    assert_eq!(output_summary["tool_name"], "cargo_check");
    assert_eq!(
        output_summary["max_excerpt_chars"],
        MAX_VALIDATION_EXCERPT_CHARS
    );
    assert!(stdout_excerpt.contains("safe stdout line"));
    assert!(!stdout_excerpt.contains("supersecret"));
    assert!(stderr_excerpt.contains("error[E0308]"));
    assert!(stderr_excerpt.contains("--> src/lib.rs:12:5"));
    assert!(!stderr_excerpt.contains("Authorization"));
    assert!(!stderr_excerpt.contains("supersecret"));
    assert!(stdout_excerpt.chars().count() <= MAX_VALIDATION_EXCERPT_CHARS);
    assert!(stderr_excerpt.chars().count() <= MAX_VALIDATION_EXCERPT_CHARS);
    assert_eq!(output_summary["stdout_truncated"], true);
    assert_eq!(output_summary["stderr_truncated"], true);

    let serialized = serde_json::to_string(finished).unwrap();
    for leaked in [
        "full stdout body must not be ledgered",
        "full stderr body must not be ledgered",
        "api_key=supersecret",
        "Authorization: Bearer supersecret",
    ] {
        assert!(
            !serialized.contains(leaked),
            "session event leaked unsafe validation output {leaked}: {serialized}"
        );
    }
    for raw_key in [
        "\"stdout\":",
        "\"stderr\":",
        "\"stdout_tail\":",
        "\"stderr_tail\":",
    ] {
        assert!(
            !serialized.contains(raw_key),
            "session event stored raw output key {raw_key}: {serialized}"
        );
    }
}

#[test]
fn validation_summary_wires_cargo_check_diagnostics_from_captured_excerpt() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_check",
        json!({"project": "agent:eval:demo"}),
        false,
        json!({
            "exit_code": 101,
            "stdout_tail": "",
            "stderr_tail": "error[E0308]: mismatched types\n --> src/lib.rs:12:5\n",
            "stdout_truncated": false,
            "stderr_truncated": false,
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);
    let diagnostics = &validation["latest_failure"]["diagnostics"];

    assert_eq!(validation["parser"]["available"], true);
    assert_eq!(validation["parser"]["kind"], PARSER_KIND);
    assert!(validation["parser"].get("reason").is_none());
    assert_eq!(diagnostics["available"], true);
    assert_eq!(diagnostics["parser"], PARSER_KIND);
    assert_eq!(diagnostics["diagnostic_count"], 1);
    assert_eq!(diagnostics["returned_diagnostic_count"], 1);
    assert_eq!(diagnostics["diagnostics_truncated"], false);
    assert_eq!(diagnostics["invalid_diagnostics_omitted"], 0);
    assert_eq!(diagnostics["diagnostics"].as_array().unwrap().len(), 1);
    assert_eq!(diagnostics["diagnostics"][0]["severity"], "error");
    assert_eq!(diagnostics["diagnostics"][0]["code"], "E0308");
    assert_eq!(diagnostics["diagnostics"][0]["file"], "src/lib.rs");
    assert_eq!(diagnostics["diagnostics"][0]["line"], 12);
    assert_eq!(diagnostics["diagnostics"][0]["column"], 5);
    assert_eq!(diagnostics["diagnostics"][0]["message"], "mismatched types");
    assert_eq!(
        validation["latest_failure"]["failure_kind"],
        "compile_error"
    );
    assert_eq!(diagnostics["truncated"], false);
    assert_no_raw_validation_output_fields(&validation, "validation summary");
}

#[test]
fn validation_summary_wires_cargo_test_summary_from_captured_excerpt() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo"}),
        false,
        json!({
            "exit_code": 101,
            "stdout_tail": "running 1 test\ntest tests::fails ... FAILED\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);
    let diagnostics = &validation["latest_failure"]["diagnostics"];

    assert_eq!(validation["parser"]["available"], true);
    assert_eq!(diagnostics["available"], true);
    assert_eq!(diagnostics["diagnostic_count"], 1);
    assert_eq!(diagnostics["test_summary"]["passed"], 0);
    assert_eq!(diagnostics["test_summary"]["failed"], 1);
    assert_eq!(diagnostics["test_summary"]["ignored"], 0);
    assert_eq!(
        diagnostics["failed_test_details"][0]["name"],
        "tests::fails"
    );
    assert_eq!(
        diagnostics["failed_test_details"][0]["failure_kind"],
        "unknown"
    );
    assert_eq!(diagnostics["failed_test_details_truncated"], false);
    assert_eq!(diagnostics["truncated"], false);
    assert_eq!(validation["latest_failure"]["failure_kind"], "test_failure");
}

#[test]
fn validation_failure_kind_prefers_safe_metadata_and_specific_evidence() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    for (tool, output, expected) in [
        (
            "cargo_test",
            json!({
                "exit_code": 101,
                "stdout_tail": "",
                "stderr_tail": "error[E0308]: mismatched types\n --> src/lib.rs:2:1\n",
                "stdout_truncated": false,
                "stderr_truncated": false
            }),
            "compile_error",
        ),
        (
            "cargo_test",
            json!({
                "exit_code": -1,
                "failure_kind": "timeout",
                "stdout_tail": "test tests::name ... FAILED\n",
                "stderr_tail": "",
                "stdout_truncated": false,
                "stderr_truncated": false
            }),
            "timeout",
        ),
        (
            "cargo_fmt",
            json!({
                "exit_code": 1,
                "stdout_tail": "Diff in src/lib.rs:1:\n-old\n+new\n",
                "stderr_tail": "",
                "stdout_truncated": false,
                "stderr_truncated": false
            }),
            "format_diff",
        ),
        (
            "cargo_check",
            json!({
                "exit_code": 2,
                "stdout_tail": "",
                "stderr_tail": "process exited without diagnostics\n",
                "stdout_truncated": false,
                "stderr_truncated": false
            }),
            "process_exit",
        ),
    ] {
        record_finished_tool(
            &store,
            &session.session_id,
            tool,
            json!({"project": "agent:eval:demo"}),
            false,
            output,
        );
        let summary = store.summary(&session.session_id, Some(100)).unwrap();
        let validation = validation_summary_for_session(&summary);
        assert_eq!(validation["latest"]["failure_kind"], expected, "{tool}");
    }
}

#[test]
fn zero_tests_success_is_not_classified_as_test_failure() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo"}),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 0,
            "zero_tests_run": true
        }),
    );

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["latest"]["failure_kind"], "unknown");
}

#[test]
fn validation_summary_exposes_failed_test_details_on_latest_and_latest_failure() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo"}),
        false,
        json!({
            "exit_code": 101,
            "stdout_tail": "test tests::first ... FAILED\n\
        test tests::second ... FAILED\n\
        test tests::third ... FAILED\n\
        test result: FAILED. 7 passed; 3 failed; 1 ignored\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);
    let expected_names = ["tests::first", "tests::second", "tests::third"];

    for path in ["latest", "latest_failure"] {
        let diagnostics = &validation[path]["diagnostics"];
        assert_eq!(diagnostics["available"], true, "{path}");
        assert_eq!(diagnostics["diagnostic_count"], 3, "{path}");
        let names: Vec<&str> = diagnostics["failed_test_details"]
            .as_array()
            .unwrap()
            .iter()
            .map(|detail| detail["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, expected_names, "{path}");
        assert_eq!(
            diagnostics["failed_test_details_truncated"], false,
            "{path}"
        );
        assert_eq!(diagnostics["test_summary"]["failed"], 3, "{path}");
        assert_eq!(diagnostics["truncated"], false, "{path}");
    }
}

#[test]
fn cargo_test_run_metadata_counts_only_executed_tests() {
    let metadata = parse_cargo_test_run_metadata(
        "running 7 tests\n\
         test result: ok. 1 passed; 0 failed; 3 ignored; 2 measured; 4 filtered out\n",
    );

    assert!(metadata.tests_detected);
    assert_eq!(metadata.tests_run_count, Some(1));
    assert_eq!(metadata.zero_tests_run, Some(false));
    assert_eq!(metadata.count_evidence_reason, "complete_summary");

    let measured_only = parse_cargo_test_run_metadata(
        "running 2 tests\n\
         test result: ok. 0 passed; 0 failed; 0 ignored; 2 measured; 0 filtered out\n",
    );
    assert_eq!(measured_only.tests_run_count, Some(0));
    assert_eq!(measured_only.zero_tests_run, Some(true));
}

#[test]
fn cargo_test_run_metadata_reports_ignored_only_as_zero_executed() {
    let metadata = parse_cargo_test_run_metadata(
        "running 1 test\n\
         test ignored_only ... ignored\n\
         test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\n",
    );

    assert!(metadata.tests_detected);
    assert_eq!(metadata.tests_run_count, Some(0));
    assert_eq!(metadata.zero_tests_run, Some(true));
}

#[test]
fn cargo_test_run_metadata_counts_failed_tests_as_executed() {
    let metadata = parse_cargo_test_run_metadata(
        "running 3 tests\n\
         test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
    );

    assert!(metadata.tests_detected);
    assert_eq!(metadata.tests_run_count, Some(3));
    assert_eq!(metadata.zero_tests_run, Some(false));
}

#[test]
fn cargo_test_run_metadata_aggregates_complete_harness_summaries() {
    let metadata = parse_cargo_test_run_metadata(
        "running 5 tests\n\
         test result: ok. 2 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out\n\n\
         running 6 tests\n\
         test result: FAILED. 1 passed; 1 failed; 4 ignored; 0 measured; 0 filtered out\n",
    );

    assert!(metadata.tests_detected);
    assert_eq!(metadata.tests_run_count, Some(4));
    assert_eq!(metadata.tests_passed, Some(3));
    assert_eq!(metadata.tests_failed, Some(1));
    assert_eq!(metadata.zero_tests_run, Some(false));
}

#[test]
fn cargo_test_run_metadata_keeps_positive_aggregate_when_last_harness_is_zero() {
    let metadata = parse_cargo_test_run_metadata(
        "running 2788 tests\n\
         test result: ok. 2788 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n\n\
         running 0 tests\n\
         test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
    );
    assert_eq!(metadata.tests_passed, Some(2788));
    assert_eq!(metadata.tests_failed, Some(0));
    assert_eq!(metadata.tests_run_count, Some(2788));
    assert_eq!(metadata.zero_tests_run, Some(false));
    assert_eq!(metadata.count_evidence_reason, "complete_summary");
}

#[test]
fn cargo_test_run_metadata_reports_precise_unproven_reason() {
    let real_success = parse_cargo_test_run_metadata(
        "running 224 tests\n\
         test result: ok. 224 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
    );
    assert_eq!(real_success.tests_run_count, Some(224));
    assert_eq!(real_success.count_evidence_reason, "complete_summary");

    let partial_after_success = parse_cargo_test_run_metadata(
        "running 224 tests\n\
         test result: ok. 224 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n\
         test result: ok. 0 passed;\n",
    );
    assert_eq!(partial_after_success.tests_run_count, None);
    assert_eq!(
        partial_after_success.count_evidence_reason,
        "partial_harness_summary"
    );

    let no_complete = parse_cargo_test_run_metadata("running 224 tests\n");
    assert_eq!(no_complete.tests_run_count, None);
    assert_eq!(no_complete.count_evidence_reason, "no_complete_summary");
}

#[test]
fn cargo_test_run_metadata_does_not_guess_from_running_or_partial_summary() {
    for output in [
        "running 10 tests\n",
        "running 10 tests\ntest result: ok. 10 passed;\n",
        "test result: ok. 10 passed; 0 failed\n",
        "test result: unknown. 10 passed; 0 failed; 0 ignored\n",
        "test result: FAILED. 2 passed; 1 failed; 0 ignored\n\
         test result: ok. 4 passed;\n",
    ] {
        let metadata = parse_cargo_test_run_metadata(output);
        assert!(metadata.tests_detected, "{output:?}");
        assert_eq!(metadata.tests_run_count, None, "{output:?}");
        assert_eq!(metadata.tests_passed, None, "{output:?}");
        assert_eq!(metadata.tests_failed, None, "{output:?}");
        assert_eq!(metadata.zero_tests_run, None, "{output:?}");
    }
}

#[test]
fn cargo_test_session_metadata_preserves_explicit_unproven_count_authority() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo", "filter": "focused"}),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "test result: ok. 10 passed; 0 failed\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": null,
            "zero_tests_run": null
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);
    let event = &validation["latest"];

    assert_eq!(event["success"], true);
    assert_eq!(event["tests_detected"], true);
    assert!(event["tests_run_count"].is_null());
    assert!(event["zero_tests_run"].is_null());
    assert!(event["detected_summary"]["tests_run_count"].is_null());
    assert!(event["detected_summary"]["zero_tests_run"].is_null());
    assert_eq!(event["diagnostics"]["test_summary"]["passed"], 10);
    assert_eq!(event["diagnostics"]["test_summary"]["failed"], 0);
}

#[test]
fn legacy_persisted_cargo_test_metadata_absence_uses_diagnostics_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = dir.path().join("sessions.json");
    let store = SessionStore::with_persistence(&ledger, 10, 10);
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo", "filter": "legacy"}),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "test result: ok. 2 passed; 0 failed; 0 ignored\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 2,
            "zero_tests_run": false
        }),
    );
    store.flush_persistence();
    drop(store);

    let mut ledger_json: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger).unwrap()).unwrap();
    let events = ledger_json["sessions"][0]["events"].as_array_mut().unwrap();
    let finished = events
        .iter_mut()
        .find(|event| event["kind"] == "tool_call_finished" && event["tool_name"] == "cargo_test")
        .unwrap();
    let output_summary = finished["validation_output_summary"]
        .as_object_mut()
        .unwrap();
    output_summary.remove("tests_run_count");
    output_summary.remove("zero_tests_run");
    std::fs::write(&ledger, serde_json::to_vec(&ledger_json).unwrap()).unwrap();

    let restored = SessionStore::with_persistence(&ledger, 10, 10);
    let session = restored.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);
    let event = &validation["latest"];

    assert_eq!(event["tests_detected"], true);
    assert_eq!(event["tests_run_count"], 2);
    assert_eq!(event["zero_tests_run"], false);
    assert_eq!(event["diagnostics"]["test_summary"]["passed"], 2);
    assert_eq!(event["diagnostics"]["test_summary"]["failed"], 0);
}

#[test]
fn validation_summary_exposes_cargo_test_zero_tests_metadata() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo"}),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 0,
            "zero_tests_run": true
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);
    let event = &validation["latest"];

    assert_eq!(validation["status"], "inconclusive");
    assert_eq!(validation["reason"], "zero_tests_not_validation_proof");
    assert_eq!(validation["successes"], 0);
    assert_eq!(validation["latest_status"], "inconclusive");
    assert!(validation.get("latest_success").is_none());
    assert_eq!(validation["current_evidence"]["status"], "inconclusive");
    assert_eq!(validation["current_evidence"]["successes"], 0);
    assert_eq!(validation["cargo_test_zero_tests_run"], true);
    assert_eq!(validation["historical_failures"]["count"], 0);
    assert_eq!(validation["historical_failures"]["resolved"], false);
    assert_eq!(validation["historical_failures"]["unresolved"], false);
    assert_eq!(event["tool_name"], "cargo_test");
    assert_eq!(event["tests_detected"], true);
    assert_eq!(event["tests_run_count"], 0);
    assert_eq!(event["zero_tests_run"], true);
    assert_eq!(event["success"], true);
    assert_no_raw_validation_output_fields(&validation, "validation summary");
}

#[test]
fn successful_cargo_test_with_executed_tests_is_proven_validation() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo"}),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 2 tests\n\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 2,
            "zero_tests_run": false
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);

    assert_eq!(validation["status"], "passed");
    assert_eq!(validation["successes"], 1);
    assert_eq!(validation["latest_status"], "passed");
    assert_eq!(validation["latest_success"]["tests_run_count"], 2);
    assert_eq!(validation["current_evidence"]["status"], "passed");
}

#[test]
fn explicit_require_tests_false_accepts_zero_test_validation_proof() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo", "require_tests": false}),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 0,
            "zero_tests_run": true,
            "require_tests": false
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);

    assert_eq!(validation["status"], "passed");
    assert_eq!(validation["successes"], 1);
    assert_eq!(validation["latest_status"], "passed");
    assert_eq!(validation["latest_success"]["require_tests"], false);
    assert_eq!(validation["latest_success"]["zero_tests_run"], true);
    assert_eq!(validation["current_evidence"]["status"], "passed");
}

#[test]
fn cargo_test_no_run_does_not_require_executed_test_count_proof() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo", "no_run": true}),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": false,
            "tests_run_count": null,
            "zero_tests_run": null,
            "no_run": true
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);

    assert_eq!(validation["status"], "passed");
    assert_eq!(validation["successes"], 1);
    assert_eq!(validation["latest_status"], "passed");
    assert_eq!(validation["latest_success"]["no_run"], true);
}

#[test]
fn missing_test_count_metadata_is_inconclusive_not_zero_tests() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo"}),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "test result: ok. 10 passed; 0 failed\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": null,
            "zero_tests_run": null
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);

    assert_eq!(validation["status"], "inconclusive");
    assert_eq!(validation["reason"], "test_count_metadata_unavailable");
    assert_eq!(validation["successes"], 0);
    assert_eq!(validation["latest_status"], "inconclusive");
    assert!(validation["latest"]["tests_run_count"].is_null());
    assert!(validation["latest"]["zero_tests_run"].is_null());
    assert_eq!(validation["cargo_test_zero_tests_run"], false);
}

#[test]
fn later_zero_test_event_does_not_replace_previous_real_validation_proof() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    let arguments = json!({"project": "agent:eval:demo"});
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        arguments.clone(),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 2 tests\n\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 2,
            "zero_tests_run": false
        }),
    );
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        arguments,
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 0,
            "zero_tests_run": true
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);

    assert_eq!(validation["status"], "passed");
    assert_eq!(validation["successes"], 1);
    assert_eq!(validation["latest_status"], "inconclusive");
    assert_eq!(validation["latest"]["zero_tests_run"], true);
    assert_eq!(validation["latest_success"]["tests_run_count"], 2);
    assert_eq!(validation["current_evidence"]["status"], "passed");
    assert_eq!(
        validation["current_evidence"]["latest_status"],
        "inconclusive"
    );
    assert_eq!(validation["current_evidence"]["successes"], 1);
}

#[test]
fn run_shell_without_declared_validation_purpose_is_not_validation_evidence() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "run_shell",
        json!({"project": "agent:eval:demo", "command": "cargo check"}),
        false,
        json!({
            "exit_code": 101,
            "stdout_tail": "error[E0308]: must not classify shell output\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let finished = session
        .events
        .iter()
        .find(|event| event.kind == "tool_call_finished")
        .unwrap();
    assert!(finished.validation_output_summary.is_some());
    let validation = validation_summary_for_session(&session);
    assert_eq!(validation["available"], false);
    assert_eq!(validation["status"], "not_run");
    assert_eq!(validation["reason"], "no_validation_tool_invoked");
    assert_eq!(validation["events_total"], 0);
    assert_eq!(validation["parser"]["available"], false);
    assert_eq!(
        validation["parser"]["reason"],
        VALIDATION_OUTPUT_METADATA_ABSENT_REASON
    );
}

#[test]
fn unified_diff_mutation_does_not_create_validation_lifecycle() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "apply_unified_diff",
        json!({
            "project": "agent:eval:demo",
            "diff_present": true,
            "deny_sensitive_paths": true,
        }),
        true,
        json!({"applied": true, "can_apply": true}),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);
    assert_eq!(validation["available"], false);
    assert_eq!(validation["status"], "not_run");
    assert_eq!(validation["events_total"], 0);
    assert_eq!(validation["reason"], "no_validation_tool_invoked");
}

#[test]
fn latest_success_and_failure_follow_session_ledger_order() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo"}),
        false,
        json!({"exit_code": 101, "stdout": "omitted", "stderr": "omitted"}),
    );
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_check",
        json!({"project": "agent:eval:demo"}),
        true,
        json!({"exit_code": 0, "stdout": "omitted", "stderr": "omitted"}),
    );
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo"}),
        false,
        json!({"exit_code": 101}),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);

    assert_eq!(validation["available"], true);
    assert_eq!(validation["status"], "mixed");
    assert!(validation["reason"].is_null());
    assert_eq!(validation["events_total"], 3);
    assert_eq!(validation["successes"], 1);
    assert_eq!(validation["failures"], 2);
    assert_eq!(validation["latest_success"]["tool_name"], "cargo_check");
    assert_eq!(validation["latest_success"]["exit_code"], 0);
    assert_eq!(validation["latest_failure"]["tool_name"], "cargo_test");
    assert_eq!(validation["latest_failure"]["exit_code"], 101);
    assert!(validation["latest_failure"].get("diagnostics").is_none());
    assert_eq!(validation["parser"]["available"], false);
    assert_eq!(
        validation["parser"]["reason"],
        VALIDATION_OUTPUT_METADATA_ABSENT_REASON
    );
    assert_no_raw_validation_output_fields(&validation, "validation summary");
}

#[test]
fn different_validation_identity_success_does_not_resolve_failure() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo"}),
        false,
        json!({"exit_code": 101}),
    );
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_check",
        json!({"project": "agent:eval:demo"}),
        true,
        json!({"exit_code": 0}),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);

    assert_eq!(validation["available"], true);
    assert_eq!(validation["status"], "mixed");
    assert_eq!(validation["latest_status"], "passed");
    assert_eq!(validation["historical_failures"]["count"], 1);
    assert_eq!(validation["historical_failures"]["resolved"], false);
    assert_eq!(validation["historical_failures"]["unresolved"], true);
    assert_eq!(validation["resolved_failures"]["count"], 0);
    assert_eq!(validation["unresolved_failures"]["count"], 1);
    assert_eq!(validation["latest"]["tool_name"], "cargo_check");
    assert_eq!(validation["latest"]["success"], true);
    assert_eq!(validation["latest_success"]["tool_name"], "cargo_check");
    assert_eq!(validation["latest_failure"]["tool_name"], "cargo_test");
}

#[test]
fn failed_cargo_test_followed_by_zero_tests_remains_historically_unresolved() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo"}),
        false,
        json!({"exit_code": 101}),
    );
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo"}),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 0,
            "zero_tests_run": true
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);

    assert_eq!(validation["status"], "failed");
    assert_eq!(validation["successes"], 0);
    assert_eq!(validation["latest_status"], "inconclusive");
    assert!(validation.get("latest_success").is_none());
    assert_eq!(validation["latest"]["tool_name"], "cargo_test");
    assert_eq!(validation["latest"]["zero_tests_run"], true);
    assert_eq!(validation["cargo_test_zero_tests_run"], true);
    assert_eq!(validation["historical_failures"]["count"], 1);
    assert_eq!(validation["historical_failures"]["resolved"], false);
    assert_eq!(validation["historical_failures"]["unresolved"], true);
}

#[test]
fn failed_cargo_test_followed_by_unproven_success_remains_historically_unresolved() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    let arguments = json!({"project": "agent:eval:demo", "filter": "focused"});
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        arguments.clone(),
        false,
        json!({"exit_code": 101}),
    );
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        arguments,
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "test result: ok. 10 passed; 0 failed\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": null,
            "zero_tests_run": null
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);

    assert_eq!(validation["status"], "failed");
    assert_eq!(validation["successes"], 0);
    assert_eq!(validation["latest_status"], "inconclusive");
    assert!(validation.get("latest_success").is_none());
    assert_eq!(validation["latest"]["success"], true);
    assert!(validation["latest"]["tests_run_count"].is_null());
    assert!(validation["latest"]["zero_tests_run"].is_null());
    assert_eq!(
        validation["latest"]["diagnostics"]["test_summary"]["passed"],
        10
    );
    assert_eq!(
        validation["latest"]["diagnostics"]["test_summary"]["failed"],
        0
    );
    assert_eq!(validation["historical_failures"]["count"], 1);
    assert_eq!(validation["historical_failures"]["resolved"], false);
    assert_eq!(validation["historical_failures"]["unresolved"], true);
    assert_eq!(validation["resolved_failures"]["count"], 0);
    assert_eq!(validation["unresolved_failures"]["count"], 1);
}

#[test]
fn different_check_success_after_zero_tests_does_not_resolve_test_failure() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo"}),
        false,
        json!({"exit_code": 101}),
    );
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo"}),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 0,
            "zero_tests_run": true
        }),
    );
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_check",
        json!({"project": "agent:eval:demo"}),
        true,
        json!({"exit_code": 0}),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);

    assert_eq!(validation["status"], "mixed");
    assert_eq!(validation["latest_status"], "passed");
    assert_eq!(validation["historical_failures"]["count"], 1);
    assert_eq!(validation["historical_failures"]["resolved"], false);
    assert_eq!(validation["historical_failures"]["unresolved"], true);
    assert_eq!(validation["resolved_failures"]["count"], 0);
    assert_eq!(validation["unresolved_failures"]["count"], 1);
}

#[test]
fn same_validation_identity_success_resolves_failure_without_deleting_history() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo"}),
        false,
        json!({"exit_code": 101}),
    );
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo"}),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 1 test\n\ntest result: ok. 1 passed; 0 failed; 0 ignored\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 1,
            "zero_tests_run": false
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);

    assert_eq!(validation["historical_failures"]["count"], 1);
    assert_eq!(validation["resolved_failures"]["count"], 1);
    assert_eq!(validation["unresolved_failures"]["count"], 0);
    assert_eq!(
        validation["resolved_failures"]["events"][0]["unresolved_failure"],
        false
    );
    assert_eq!(validation["events_total"], 2);
}

#[test]
fn generic_cargo_test_zero_tests_is_inconclusive_without_failing_execution() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    let input = session_log_arguments_for_tool_request(
        "run_process",
        &json!({
            "project": "agent:eval:demo",
            "executable": "cargo",
            "args": ["test", "focused", "-p", "webcodex"],
            "cwd": ".",
            "purpose": "test"
        }),
    );
    assert_eq!(input["validation_tool"], "cargo_test");

    record_finished_tool(
        &store,
        &session.session_id,
        "run_process",
        input,
        true,
        json!({
            "exit_code": 0,
            "purpose": "test",
            "execution_state": "completed",
            "stdout_tail": "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 0,
            "zero_tests_run": true
        }),
    );

    let validation =
        validation_summary_for_session(&store.summary(&session.session_id, Some(50)).unwrap());
    assert_eq!(validation["status"], "inconclusive");
    assert_eq!(validation["reason"], "zero_tests_not_validation_proof");
    assert_eq!(validation["successes"], 0);
    assert_eq!(validation["latest_status"], "inconclusive");
    assert!(validation.get("latest_success").is_none());
    assert_eq!(validation["cargo_test_zero_tests_run"], true);
    assert_eq!(validation["latest"]["tool_name"], "run_process");
    assert_eq!(validation["latest"]["success"], true);
    assert_eq!(validation["latest"]["exit_code"], 0);
    assert_eq!(validation["latest"]["tests_run_count"], 0);
    assert_eq!(validation["latest"]["zero_tests_run"], true);
}

#[test]
fn generic_cargo_test_zero_tests_does_not_resolve_same_identity_failure() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    let input = session_log_arguments_for_tool_request(
        "run_process",
        &json!({
            "project": "agent:eval:demo",
            "executable": "cargo",
            "args": ["test", "focused", "-p", "webcodex"],
            "cwd": ".",
            "purpose": "test"
        }),
    );

    record_finished_tool(
        &store,
        &session.session_id,
        "run_process",
        input.clone(),
        false,
        json!({
            "exit_code": 101,
            "purpose": "test",
            "execution_state": "completed",
            "stdout_tail": "generic cargo test failed\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false
        }),
    );
    record_finished_tool(
        &store,
        &session.session_id,
        "run_process",
        input,
        true,
        json!({
            "exit_code": 0,
            "purpose": "test",
            "execution_state": "completed",
            "stdout_tail": "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 0,
            "zero_tests_run": true
        }),
    );

    let validation =
        validation_summary_for_session(&store.summary(&session.session_id, Some(50)).unwrap());
    assert_eq!(validation["successes"], 0);
    assert_eq!(validation["latest_status"], "inconclusive");
    assert!(validation.get("latest_success").is_none());
    assert_eq!(validation["historical_failures"]["count"], 1);
    assert_eq!(validation["historical_failures"]["resolved"], false);
    assert_eq!(validation["historical_failures"]["unresolved"], true);
    assert_eq!(validation["resolved_failures"]["count"], 0);
    assert_eq!(validation["unresolved_failures"]["count"], 1);
    assert_eq!(validation["cargo_test_zero_tests_run"], true);
}

#[test]
fn generic_test_success_without_structured_counts_resolves_same_identity_failure() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    let input = session_log_arguments_for_tool_request(
        "run_process",
        &json!({
            "project": "agent:eval:demo",
            "executable": "cargo",
            "args": ["test", "focused", "-p", "webcodex"],
            "cwd": ".",
            "purpose": "test"
        }),
    );
    let identity = input["validation_target_id"]
        .as_str()
        .expect("generic validation identity")
        .to_string();
    assert_eq!(input["validation_tool"], "cargo_test");

    record_finished_tool(
        &store,
        &session.session_id,
        "run_process",
        input.clone(),
        false,
        json!({
            "exit_code": 101,
            "purpose": "test",
            "execution_state": "completed",
            "stdout_tail": "generic harness failed\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false
        }),
    );
    record_finished_tool(
        &store,
        &session.session_id,
        "run_process",
        input,
        true,
        json!({
            "exit_code": 0,
            "purpose": "test",
            "execution_state": "completed",
            "stdout_tail": "generic harness passed\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false
        }),
    );

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let raw_finished = summary
        .events
        .iter()
        .filter(|event| event.kind == "tool_call_finished" && event.tool_name == "run_process")
        .collect::<Vec<_>>();
    assert_eq!(raw_finished.len(), 2);
    assert_eq!(raw_finished[0].status.as_deref(), Some("failed"));
    assert_eq!(raw_finished[1].status.as_deref(), Some("succeeded"));

    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["events_total"], 2);
    assert_eq!(validation["status"], "mixed");
    assert_eq!(validation["latest_status"], "passed");
    assert_eq!(validation["historical_failures"]["count"], 1);
    assert_eq!(validation["historical_failures"]["resolved"], true);
    assert_eq!(validation["historical_failures"]["unresolved"], false);
    assert_eq!(validation["resolved_failures"]["count"], 1);
    assert_eq!(validation["unresolved_failures"]["count"], 0);
    assert_eq!(validation["latest"]["execution_source"], "run_process");
    assert_eq!(validation["latest"]["validation_kind"], "test");
    assert_eq!(validation["latest"]["identity"], identity);
    assert!(validation["latest"]["tests_run_count"].is_null());
    assert!(validation["latest"]["zero_tests_run"].is_null());
}

#[test]
fn proven_generic_cargo_test_resolves_same_generic_and_structured_target_only() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    let generic_input = json!({
        "project": "agent:eval:demo", "executable": "cargo",
        "args": ["test", "focused", "-p", "webcodex"], "cwd": ".", "purpose": "test"
    });
    let generic_input = session_log_arguments_for_tool_request("run_process", &generic_input);
    let failed_output = json!({
        "exit_code": 101, "purpose": "test",
        "stdout_tail": "running 1 test\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
        "stderr_tail": "", "stdout_truncated": false, "stderr_truncated": false,
        "tests_detected": true, "tests_run_count": 1, "zero_tests_run": false
    });
    record_finished_tool(
        &store,
        &session.session_id,
        "run_process",
        generic_input.clone(),
        false,
        failed_output.clone(),
    );
    record_finished_tool(
        &store,
        &session.session_id,
        "run_process",
        generic_input.clone(),
        true,
        json!({
            "exit_code": 0, "purpose": "test",
            "stdout_tail": "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "", "stdout_truncated": false, "stderr_truncated": false,
            "tests_detected": true, "tests_run_count": 1, "zero_tests_run": false
        }),
    );
    let validation =
        validation_summary_for_session(&store.summary(&session.session_id, Some(50)).unwrap());
    assert_eq!(validation["resolved_failures"]["count"], 1);
    assert_eq!(validation["unresolved_failures"]["count"], 0);

    assert!(validation["events"]
        .as_array()
        .unwrap()
        .iter()
        .all(|event| {
            event["identity"]
                .as_str()
                .is_some_and(|identity| identity.starts_with("target:"))
        }));

    let cross = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &cross.session_id,
        "run_process",
        generic_input,
        false,
        failed_output,
    );
    record_finished_tool(
        &store,
        &cross.session_id,
        "cargo_test",
        json!({
            "project": "agent:eval:demo", "package": "webcodex", "filter": "different", "cwd": ".",
            "all_targets": false, "all_features": false, "no_default_features": false,
            "no_run": false, "features": null
        }),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "", "stdout_truncated": false, "stderr_truncated": false,
            "tests_detected": true, "tests_run_count": 1, "zero_tests_run": false
        }),
    );
    let before_structured =
        validation_summary_for_session(&store.summary(&cross.session_id, Some(50)).unwrap());
    assert_eq!(before_structured["unresolved_failures"]["count"], 1);

    record_finished_tool(
        &store,
        &cross.session_id,
        "cargo_test",
        json!({
            "project": "agent:eval:demo", "package": "webcodex", "filter": "focused", "cwd": ".",
            "all_targets": false, "all_features": false, "no_default_features": false,
            "no_run": false, "features": null
        }),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "", "stdout_truncated": false, "stderr_truncated": false,
            "tests_detected": true, "tests_run_count": 1, "zero_tests_run": false
        }),
    );
    let validation =
        validation_summary_for_session(&store.summary(&cross.session_id, Some(50)).unwrap());
    assert_eq!(validation["resolved_failures"]["count"], 1);
    assert_eq!(validation["unresolved_failures"]["count"], 0);
}

#[test]
fn generic_validation_scope_and_complex_script_identity_fail_closed() {
    use webcodex_tool_runtime_contracts::tool_audit::{
        run_process_validation_identity, run_script_validation_identity,
    };
    let x = run_process_validation_identity(
        "cargo",
        &[
            "test".into(),
            "focused".into(),
            "-p".into(),
            "webcodex".into(),
        ],
        None,
        Some("."),
        Some("test"),
    )
    .unwrap();
    let y = run_process_validation_identity(
        "cargo",
        &[
            "test".into(),
            "other".into(),
            "-p".into(),
            "webcodex".into(),
        ],
        None,
        Some("."),
        Some("test"),
    )
    .unwrap();
    assert!(x.identity.starts_with("target:"));
    assert!(y.identity.starts_with("target:"));
    assert_ne!(x.identity, y.identity);
    let complex = run_script_validation_identity(
        "bash",
        "set -e; cargo test focused; echo done",
        &[],
        None,
        Some("."),
        Some("test"),
    )
    .unwrap();
    assert!(complex.identity.starts_with("command:"));
    assert!(complex.validation_tool.is_none());
}

#[test]
fn complex_run_script_failure_keeps_generic_identity_and_cannot_be_resolved_by_structured_pass() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    let script = "set -e\ncargo test focused -p webcodex\necho done";
    let input = session_log_arguments_for_tool_request(
        "run_script",
        &json!({
            "project": "agent:eval:demo", "language": "bash", "script": script,
            "args": [], "stdin": null, "cwd": ".", "purpose": "test"
        }),
    );
    assert!(input["execution_identity"]
        .as_str()
        .is_some_and(|identity| identity.starts_with("command:")));
    assert!(input.get("validation_target_id").is_none());
    assert!(!serde_json::to_string(&input).unwrap().contains(script));
    record_finished_tool(
        &store,
        &session.session_id,
        "run_script",
        input,
        false,
        json!({
            "exit_code": 101, "purpose": "test",
            "stdout_tail": "running 1 test\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "", "stdout_truncated": false, "stderr_truncated": false,
            "tests_detected": true, "tests_run_count": 1, "tests_passed": 0,
            "tests_failed": 1, "zero_tests_run": false
        }),
    );
    let before =
        validation_summary_for_session(&store.summary(&session.session_id, Some(50)).unwrap());
    assert_eq!(before["unresolved_failures"]["count"], 1);
    assert_eq!(before["latest"]["tests_run_count"], 1);
    assert_eq!(before["latest"]["tests_passed"], 0);
    assert_eq!(before["latest"]["tests_failed"], 1);

    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({
            "project": "agent:eval:demo", "package": "webcodex", "filter": "focused", "cwd": ".",
            "all_targets": false, "all_features": false, "no_default_features": false,
            "no_run": false, "features": null
        }),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "", "stdout_truncated": false, "stderr_truncated": false,
            "tests_detected": true, "tests_run_count": 1, "tests_passed": 1,
            "tests_failed": 0, "zero_tests_run": false
        }),
    );
    let after =
        validation_summary_for_session(&store.summary(&session.session_id, Some(50)).unwrap());
    assert_eq!(after["unresolved_failures"]["count"], 1);
    assert_eq!(after["resolved_failures"]["count"], 0);
}

#[test]
fn generic_job_failure_identity_survives_restart_and_resolves_with_structured_success() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = dir.path().join("sessions.json");
    let store = SessionStore::with_persistence(&ledger, 10, 20);
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    let input = session_log_arguments_for_tool_request(
        "run_process",
        &json!({
            "project": "agent:eval:demo", "executable": "cargo",
            "args": ["test", "focused", "-p", "webcodex"], "cwd": ".", "purpose": "test"
        }),
    );
    let target = input["validation_target_id"].as_str().unwrap().to_string();
    let terminal_output = json!({
        "exit_code": 101, "purpose": "test", "validation_tool": "cargo_test",
        "stdout_tail": "running 1 test\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
        "stderr_tail": "", "stdout_truncated": false, "stderr_truncated": false,
        "tests_detected": true, "tests_run_count": 1, "tests_passed": 0,
        "tests_failed": 1, "zero_tests_run": false
    });
    let terminal_summary =
        sessions::execution_output_summary_for_tool_result("run_process", &terminal_output);
    assert!(store.record_validation_job_terminal(
        &session.session_id,
        "job_generic_failed",
        &["job_generic_failed"],
        "run_process",
        session_tool_contract("run_process"),
        Some("agent:eval:demo".to_string()),
        &target,
        None,
        "failed",
        Some(101),
        Some(false),
        Some(100),
        Some(110),
        Some(10_000),
        terminal_summary,
    ));
    let failed = validation_summary_for_session(&store.summary(&session.session_id, None).unwrap());
    assert_eq!(failed["unresolved_failures"]["count"], 1);
    assert_eq!(failed["latest"]["identity"], target);
    store.flush_persistence();
    drop(store);

    let restored = SessionStore::with_persistence(&ledger, 10, 20);
    record_finished_tool(
        &restored,
        &session.session_id,
        "cargo_test",
        json!({
            "project": "agent:eval:demo", "package": "webcodex", "filter": "focused", "cwd": ".",
            "all_targets": false, "all_features": false, "no_default_features": false,
            "no_run": false, "features": null
        }),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "", "stdout_truncated": false, "stderr_truncated": false,
            "tests_detected": true, "tests_run_count": 1, "tests_passed": 1,
            "tests_failed": 0, "zero_tests_run": false
        }),
    );
    let resolved =
        validation_summary_for_session(&restored.summary(&session.session_id, None).unwrap());
    assert_eq!(resolved["resolved_failures"]["count"], 1);
    assert_eq!(resolved["unresolved_failures"]["count"], 0);
}

#[test]
fn contradictory_legacy_cargo_counts_are_downgraded_to_unproven() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo", "filter": "focused"}),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 3 tests\ntest result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "", "stdout_truncated": false, "stderr_truncated": false,
            "tests_detected": true, "tests_run_count": 0, "zero_tests_run": true
        }),
    );
    let validation =
        validation_summary_for_session(&store.summary(&session.session_id, Some(50)).unwrap());
    assert!(validation["latest"]["tests_run_count"].is_null());
    assert!(validation["latest"]["tests_passed"].is_null());
    assert!(validation["latest"]["tests_failed"].is_null());
    assert!(validation["latest"]["zero_tests_run"].is_null());
}

#[test]
fn validation_job_terminal_identity_survives_event_eviction_and_restart_without_refreshing_activity(
) {
    let dir = tempfile::tempdir().unwrap();
    let ledger = dir.path().join("sessions.json");
    let store = SessionStore::with_persistence(&ledger, 10, 4);
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    let target = "target:aaaaaaaaaaaaaaaaaaaaaaaa";
    let job_id = "job_terminal_success";
    let retained = [job_id];
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_check",
        json!({
            "project": "agent:eval:demo",
            "validation_target_id": target,
        }),
        false,
        json!({"exit_code": 101}),
    );
    let before_materialize = store.summary(&session.session_id, None).unwrap();
    let authoritative_finished_at = before_materialize.updated_at;
    // Force reconciliation wall-clock time into a later second. Synthetic Job
    // terminal evidence must still use the authoritative execution timestamp.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(store.record_validation_job_terminal(
        &session.session_id,
        job_id,
        &retained,
        "cargo_check",
        session_tool_contract("cargo_check"),
        Some("agent:eval:demo".to_string()),
        target,
        None,
        "completed",
        Some(0),
        Some(true),
        Some(authoritative_finished_at.saturating_sub(1)),
        Some(authoritative_finished_at),
        Some(1000),
        None,
    ));

    let materialized = store.summary(&session.session_id, None).unwrap();
    assert_eq!(materialized.updated_at, authoritative_finished_at);
    let validation = validation_summary_for_session(&materialized);
    assert_eq!(validation["historical_failures"]["count"], 1);
    assert_eq!(validation["resolved_failures"]["count"], 1);
    assert_eq!(validation["unresolved_failures"]["count"], 0);
    assert_eq!(
        materialized
            .events
            .iter()
            .filter(|event| event.kind == "validation_job_terminal")
            .count(),
        1
    );

    // Push well beyond this test store's four-event retention cap. The durable
    // materialization identity must outlive the evidence event itself while the
    // authoritative Job can still be a reconciliation candidate.
    for index in 0..3 {
        record_finished_tool(
            &store,
            &session.session_id,
            "read_file",
            json!({"project": "agent:eval:demo", "path": format!("src/{index}.rs")}),
            true,
            json!({}),
        );
    }
    let after_eviction = store.summary(&session.session_id, None).unwrap();
    assert!(after_eviction
        .events
        .iter()
        .all(|event| event.kind != "validation_job_terminal"));
    let events_total_before_repeat = after_eviction.events_total;
    let updated_at_before_repeat = after_eviction.updated_at;
    assert!(
        !store.record_validation_job_terminal(
            &session.session_id,
            job_id,
            &retained,
            "cargo_check",
            session_tool_contract("cargo_check"),
            Some("agent:eval:demo".to_string()),
            target,
            None,
            "completed",
            Some(0),
            Some(true),
            Some(authoritative_finished_at.saturating_sub(1)),
            Some(authoritative_finished_at),
            Some(1000),
            None,
        ),
        "FIFO eviction must not make an authoritative terminal Job materializable again"
    );
    let after_repeat = store.summary(&session.session_id, None).unwrap();
    assert_eq!(after_repeat.events_total, events_total_before_repeat);
    assert_eq!(after_repeat.updated_at, updated_at_before_repeat);
    assert!(after_repeat
        .events
        .iter()
        .all(|event| event.kind != "validation_job_terminal"));

    store.flush_persistence();
    drop(store);
    let restored = SessionStore::with_persistence(&ledger, 10, 4);
    let restored_before_repeat = restored.summary(&session.session_id, None).unwrap();
    assert!(restored_before_repeat
        .events
        .iter()
        .all(|event| event.kind != "validation_job_terminal"));
    assert!(
        !restored.record_validation_job_terminal(
            &session.session_id,
            job_id,
            &retained,
            "cargo_check",
            session_tool_contract("cargo_check"),
            Some("agent:eval:demo".to_string()),
            target,
            None,
            "completed",
            Some(0),
            Some(true),
            Some(authoritative_finished_at.saturating_sub(1)),
            Some(authoritative_finished_at),
            Some(1000),
            None,
        ),
        "restart within authoritative Job retention must preserve idempotence"
    );
    let restored_after_repeat = restored.summary(&session.session_id, None).unwrap();
    assert_eq!(
        restored_after_repeat.events_total,
        restored_before_repeat.events_total
    );
    assert_eq!(
        restored_after_repeat.updated_at,
        restored_before_repeat.updated_at
    );
}

#[test]
fn persisted_validation_job_materialization_ids_are_additive_sanitized_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = dir.path().join("sessions.json");
    let store = SessionStore::with_persistence(&ledger, 10, 10);
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    store.flush_persistence();
    drop(store);

    let mut ledger_json: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger).unwrap()).unwrap();
    let record = ledger_json["sessions"][0].as_object_mut().unwrap();
    // Missing field remains valid for pre-feature ledgers.
    record.remove("materialized_validation_job_ids");
    std::fs::write(&ledger, serde_json::to_vec(&ledger_json).unwrap()).unwrap();
    let legacy = SessionStore::with_persistence(&ledger, 10, 10);
    assert!(legacy.summary(&session.session_id, None).is_some());
    legacy.flush_persistence();
    drop(legacy);

    let mut ledger_json: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger).unwrap()).unwrap();
    let record = ledger_json["sessions"][0].as_object_mut().unwrap();
    let mut ids = (0..70)
        .map(|index| Value::String(format!("restored-job-{index:02}")))
        .collect::<Vec<_>>();
    ids.extend([
        Value::String("bad/id".to_string()),
        Value::String(" padded-job".to_string()),
        Value::String("duplicate-job".to_string()),
        Value::String("duplicate-job".to_string()),
    ]);
    record.insert(
        "materialized_validation_job_ids".to_string(),
        Value::Array(ids),
    );
    std::fs::write(&ledger, serde_json::to_vec(&ledger_json).unwrap()).unwrap();

    let restored = SessionStore::with_persistence(&ledger, 10, 10);
    record_finished_tool(
        &restored,
        &session.session_id,
        "read_file",
        json!({"project": "agent:eval:demo", "path": "src/sanitize.rs"}),
        true,
        json!({}),
    );
    restored.flush_persistence();
    drop(restored);
    let canonical: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger).unwrap()).unwrap();
    let ids = canonical["sessions"][0]["materialized_validation_job_ids"]
        .as_array()
        .unwrap();
    assert_eq!(ids.len(), JOB_INVENTORY_MAX_TERMINAL_JOBS);
    assert!(ids
        .iter()
        .all(|value| { value.as_str().is_some_and(is_safe_job_id) }));
    assert_eq!(ids.first().and_then(Value::as_str), Some("restored-job-07"));
    assert_eq!(ids.last().and_then(Value::as_str), Some("duplicate-job"));
    assert_eq!(
        ids.iter()
            .filter(|value| value.as_str() == Some("duplicate-job"))
            .count(),
        1
    );
}

#[test]
fn structured_validation_target_resolves_equivalent_semantic_arguments() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({
            "project": "agent:eval:demo",
            "cwd": "./crate/",
            "filter": "  oversized_result  ",
            "all_targets": true,
            "all_features": false,
            "no_default_features": false,
            "features": "  serde  ",
            "package": "  webcodex  ",
            "no_run": false,
            "timeout_secs": 30,
        }),
        false,
        json!({"exit_code": 101}),
    );
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({
            "project": "agent:eval:demo",
            "cwd": "crate",
            "filter": "oversized_result",
            "all_targets": true,
            "all_features": false,
            "no_default_features": false,
            "features": "serde",
            "package": "webcodex",
            "no_run": false,
            "timeout_secs": 120,
        }),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 1 test\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 1,
            "zero_tests_run": false
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);
    assert_eq!(validation["resolved_failures"]["count"], 1);
    assert_eq!(validation["unresolved_failures"]["count"], 0);
    let identity = validation["resolved_failures"]["events"][0]["identity"]
        .as_str()
        .unwrap();
    assert!(identity.starts_with("target:"), "{identity}");
}

#[test]
fn cargo_test_target_identity_excludes_request_scoped_evidence_assertions() {
    let target =
        |arguments| structured_validation_target_identity("cargo_test", &arguments).unwrap();
    let base = target(json!({
        "cwd": ".",
        "package": "webcodex",
        "filter": "focused",
        "features": "serde",
        "no_run": false
    }));
    for assertion_variant in [
        json!({"cwd":".","package":"webcodex","filter":"focused","features":"serde","no_run":false,"require_tests":true}),
        json!({"cwd":".","package":"webcodex","filter":"focused","features":"serde","no_run":false,"require_tests":false}),
        json!({"cwd":".","package":"webcodex","filter":"focused","features":"serde","no_run":false,"min_tests":1}),
        json!({"cwd":".","package":"webcodex","filter":"focused","features":"serde","no_run":false,"require_tests":true,"min_tests":6}),
    ] {
        assert_eq!(base, target(assertion_variant));
    }

    for (label, execution_variant) in [
        (
            "cwd",
            json!({"cwd":"crate","package":"webcodex","filter":"focused","features":"serde","no_run":false}),
        ),
        (
            "package",
            json!({"cwd":".","package":"other","filter":"focused","features":"serde","no_run":false}),
        ),
        (
            "filter",
            json!({"cwd":".","package":"webcodex","filter":"other","features":"serde","no_run":false}),
        ),
        (
            "features",
            json!({"cwd":".","package":"webcodex","filter":"focused","features":"other","no_run":false}),
        ),
        (
            "no_run",
            json!({"cwd":".","package":"webcodex","filter":"focused","features":"serde","no_run":true}),
        ),
    ] {
        assert_ne!(
            base,
            target(execution_variant),
            "{label} changes execution target"
        );
    }
}

#[test]
fn cargo_test_request_scoped_assertion_failures_are_evidence_gaps_not_correctness_failures() {
    struct Case {
        label: &'static str,
        arguments: Value,
        output: Value,
        expected_reason: &'static str,
    }

    for case in [
        Case {
            label: "minimum_not_met",
            arguments: json!({
                "project": "agent:eval:demo",
                "filter": "focused",
                "min_tests": 2,
            }),
            output: json!({
                "exit_code": 0,
                "execution_state": "completed",
                "stdout_tail": "running 1 test\ntest result: ok. 1 passed; 0 failed\n",
                "stderr_tail": "",
                "stdout_truncated": false,
                "stderr_truncated": false,
                "tests_detected": true,
                "tests_run_count": 1,
                "tests_passed": 1,
                "tests_failed": 0,
                "zero_tests_run": false,
                "test_count_assertion": {
                    "status": "failed",
                    "reason_code": "minimum_not_met",
                    "minimum_tests": 2,
                    "actual_tests_run": 1,
                    "evidence_reason_code": "complete_summary"
                }
            }),
            expected_reason: "validation_evidence_assertion_insufficient",
        },
        Case {
            label: "zero_tests_required",
            arguments: json!({
                "project": "agent:eval:demo",
                "filter": "focused",
                "require_tests": true,
            }),
            output: json!({
                "exit_code": 0,
                "execution_state": "completed",
                "stdout_tail": "running 0 tests\ntest result: ok. 0 passed; 0 failed\n",
                "stderr_tail": "",
                "stdout_truncated": false,
                "stderr_truncated": false,
                "tests_detected": true,
                "tests_run_count": 0,
                "tests_passed": 0,
                "tests_failed": 0,
                "zero_tests_run": true,
                "test_count_assertion": {
                    "status": "failed",
                    "reason_code": "minimum_not_met",
                    "minimum_tests": 1,
                    "actual_tests_run": 0,
                    "evidence_reason_code": "complete_summary"
                }
            }),
            expected_reason: "validation_evidence_assertion_insufficient",
        },
        Case {
            label: "test_count_unproven",
            arguments: json!({
                "project": "agent:eval:demo",
                "filter": "focused",
                "min_tests": 2,
            }),
            output: json!({
                "exit_code": 0,
                "execution_state": "completed",
                "stdout_tail": "running tests\n",
                "stderr_tail": "",
                "stdout_truncated": true,
                "stderr_truncated": false,
                "test_count_assertion": {
                    "status": "unproven",
                    "reason_code": "test_count_unproven",
                    "minimum_tests": 2,
                    "evidence_reason_code": "output_truncated"
                }
            }),
            expected_reason: "test_count_assertion_unproven",
        },
    ] {
        let store = SessionStore::default();
        let session = store.start_session(Some("agent:eval:demo".to_string()), None);
        record_finished_tool(
            &store,
            &session.session_id,
            "cargo_test",
            case.arguments,
            false,
            case.output,
        );

        let summary = store.summary(&session.session_id, Some(50)).unwrap();
        let raw = summary
            .events
            .iter()
            .find(|event| event.kind == "tool_call_finished")
            .expect("immutable finished cargo_test event");
        assert_eq!(raw.status.as_deref(), Some("failed"), "{}", case.label);

        let validation = validation_summary_for_session(&summary);
        assert_eq!(validation["status"], "inconclusive", "{}", case.label);
        assert_eq!(
            validation["historical_failures"]["count"], 0,
            "{}",
            case.label
        );
        assert_eq!(
            validation["resolved_failures"]["count"], 0,
            "{}",
            case.label
        );
        assert_eq!(
            validation["unresolved_failures"]["count"], 0,
            "{}",
            case.label
        );
        assert_eq!(validation["evidence_gaps"]["count"], 1, "{}", case.label);
        assert_eq!(validation["latest"]["success"], false, "{}", case.label);
        assert_eq!(
            validation["latest"]["execution_success"], true,
            "{}",
            case.label
        );
        assert_eq!(
            validation["latest"]["validation_passed"], true,
            "{}",
            case.label
        );
        assert_eq!(
            validation["latest"]["failure_class"], "evidence_assertion",
            "{}",
            case.label
        );
        assert_eq!(
            validation["latest"]["unresolved_failure"], false,
            "{}",
            case.label
        );
        assert_eq!(validation["reason"], case.expected_reason, "{}", case.label);
        assert_eq!(
            validation["current_evidence"]["status"], "inconclusive",
            "{}",
            case.label
        );
        assert_eq!(
            validation["current_evidence"]["unresolved_failure_count"], 0,
            "{}",
            case.label
        );
        assert_eq!(
            validation["current_evidence"]["evidence_gap_event_count"], 1,
            "{}",
            case.label
        );

        let current = current_validation_evidence_for_session(&summary, 50);
        assert!(
            current
                .non_actionable_tool_failure_event_ids
                .contains(&raw.event_id),
            "{} request-scoped assertion failure should not be a correctness ToolFailure blocker",
            case.label
        );
    }
}

#[test]
fn later_sufficient_same_target_evidence_closes_prior_minimum_not_met_without_rewriting_history() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    let common = json!({
        "project": "agent:eval:demo",
        "cwd": ".",
        "package": "webcodex",
        "filter": "focused",
        "features": "serde",
        "no_run": false,
    });

    let mut too_strong = common.clone();
    too_strong["min_tests"] = json!(2);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        too_strong,
        false,
        json!({
            "exit_code": 0,
            "execution_state": "completed",
            "stdout_tail": "running 1 test\ntest result: ok. 1 passed; 0 failed\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 1,
            "tests_passed": 1,
            "tests_failed": 0,
            "zero_tests_run": false,
            "test_count_assertion": {
                "status": "failed",
                "reason_code": "minimum_not_met",
                "minimum_tests": 2,
                "actual_tests_run": 1,
                "evidence_reason_code": "complete_summary"
            }
        }),
    );
    let first_summary = store.summary(&session.session_id, Some(50)).unwrap();
    let first_failed = first_summary
        .events
        .iter()
        .find(|event| event.kind == "tool_call_finished")
        .expect("first immutable failure")
        .clone();

    let mut sufficient = common;
    sufficient["min_tests"] = json!(1);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        sufficient,
        true,
        json!({
            "exit_code": 0,
            "execution_state": "completed",
            "stdout_tail": "running 1 test\ntest result: ok. 1 passed; 0 failed\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 1,
            "tests_passed": 1,
            "tests_failed": 0,
            "zero_tests_run": false,
            "test_count_assertion": {
                "status": "passed",
                "reason_code": "minimum_satisfied",
                "minimum_tests": 1,
                "actual_tests_run": 1,
                "evidence_reason_code": "complete_summary"
            }
        }),
    );

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let finished = summary
        .events
        .iter()
        .filter(|event| event.kind == "tool_call_finished")
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 2);
    assert_eq!(finished[0].event_id, first_failed.event_id);
    assert_eq!(finished[0].status.as_deref(), Some("failed"));
    assert_eq!(
        finished[0]
            .validation_output_summary
            .as_ref()
            .and_then(|value| value.pointer("/test_count_assertion/reason_code"))
            .and_then(Value::as_str),
        Some("minimum_not_met")
    );

    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["events_total"], 2);
    assert_eq!(validation["status"], "passed");
    assert_eq!(validation["successes"], 1);
    assert_eq!(validation["historical_failures"]["count"], 0);
    assert_eq!(validation["resolved_failures"]["count"], 0);
    assert_eq!(validation["unresolved_failures"]["count"], 0);
    assert_eq!(validation["evidence_gaps"]["count"], 1);
    assert_eq!(validation["events"][0]["success"], false);
    assert_eq!(validation["events"][0]["validation_passed"], true);
    assert_eq!(
        validation["events"][0]["failure_class"],
        "evidence_assertion"
    );
    assert_eq!(
        validation["events"][0]["identity"],
        validation["events"][1]["identity"]
    );
    assert_eq!(validation["current_evidence"]["status"], "passed");
    assert_eq!(
        validation["current_evidence"]["unresolved_failure_count"],
        0
    );
    assert_eq!(
        validation["current_evidence"]["evidence_gap_event_count"],
        1
    );

    let current = current_validation_evidence_for_session(&summary, 50);
    assert!(current
        .non_actionable_tool_failure_event_ids
        .contains(&first_failed.event_id));
}

#[test]
fn same_validation_identity_success_in_another_project_does_not_resolve_failure() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:alpha".to_string()), None);
    let shared_target = json!({
        "cwd": "crate",
        "package": "pkg",
        "filter": "focus",
        "features": "feat"
    });

    let mut failed = shared_target.clone();
    failed["project"] = json!("agent:eval:alpha");
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        failed,
        false,
        json!({"exit_code": 101}),
    );

    let mut succeeded_elsewhere = shared_target;
    succeeded_elsewhere["project"] = json!("agent:eval:bravo");
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        succeeded_elsewhere,
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 1 test\n\ntest result: ok. 1 passed; 0 failed; 0 ignored\n",
            "stderr_tail": "",
        }),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);
    assert_eq!(validation["resolved_failures"]["count"], 0);
    assert_eq!(validation["unresolved_failures"]["count"], 1);
    assert_eq!(
        validation["unresolved_failures"]["events"][0]["project"],
        "agent:eval:alpha"
    );
}

#[test]
fn structured_validation_target_does_not_cross_semantic_scope() {
    let cases = [
        (
            "cwd",
            json!({"project":"agent:eval:demo","cwd":"crate_a","package":"pkg","filter":"focus","features":"feat"}),
            json!({"project":"agent:eval:demo","cwd":"crate_b","package":"pkg","filter":"focus","features":"feat"}),
        ),
        (
            "package",
            json!({"project":"agent:eval:demo","cwd":"crate","package":"pkg_a","filter":"focus","features":"feat"}),
            json!({"project":"agent:eval:demo","cwd":"crate","package":"pkg_b","filter":"focus","features":"feat"}),
        ),
        (
            "filter",
            json!({"project":"agent:eval:demo","cwd":"crate","package":"pkg","filter":"focus_a","features":"feat"}),
            json!({"project":"agent:eval:demo","cwd":"crate","package":"pkg","filter":"focus_b","features":"feat"}),
        ),
        (
            "features",
            json!({"project":"agent:eval:demo","cwd":"crate","package":"pkg","filter":"focus","features":"feat_a"}),
            json!({"project":"agent:eval:demo","cwd":"crate","package":"pkg","filter":"focus","features":"feat_b"}),
        ),
    ];

    for (label, failed_args, successful_args) in cases {
        let store = SessionStore::default();
        let session = store.start_session(Some("agent:eval:demo".to_string()), None);
        record_finished_tool(
            &store,
            &session.session_id,
            "cargo_test",
            failed_args,
            false,
            json!({"exit_code": 101}),
        );
        record_finished_tool(
            &store,
            &session.session_id,
            "cargo_test",
            successful_args,
            true,
            json!({"exit_code": 0}),
        );

        let session = store.summary(&session.session_id, Some(50)).unwrap();
        let validation = validation_summary_for_session(&session);
        assert_eq!(
            validation["resolved_failures"]["count"], 0,
            "{label}: {validation}"
        );
        assert_eq!(
            validation["unresolved_failures"]["count"], 1,
            "{label}: {validation}"
        );
    }
}

#[test]
fn same_assertion_identity_resolves_only_its_own_failure() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    for (assertion_name, success, exit_code) in [
        ("release_check", false, 101),
        ("other_check", true, 0),
        ("release_check", true, 0),
    ] {
        record_finished_tool(
            &store,
            &session.session_id,
            "cargo_check",
            json!({
                "project": "agent:eval:demo",
                "assertion_name": assertion_name,
            }),
            success,
            json!({"exit_code": exit_code}),
        );
    }

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);
    assert_eq!(validation["historical_failures"]["count"], 1);
    assert_eq!(validation["resolved_failures"]["count"], 1);
    assert_eq!(validation["unresolved_failures"]["count"], 0);
    assert_eq!(
        validation["resolved_failures"]["events"][0]["identity"],
        assertion_validation_identity("release_check")
    );
}

#[test]
fn public_validation_assertion_label_hides_structured_and_unsafe_historical_metadata() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:label-safety".to_string()), None);
    let hidden_structured = "internal structured assertion";
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_check",
        json!({
            "project": "agent:eval:label-safety",
            "assertion_name": hidden_structured,
        }),
        true,
        json!({"exit_code": 0}),
    );
    let unsafe_legacy = "Bearer historical-validation-secret";
    record_finished_tool(
        &store,
        &session.session_id,
        "run_process",
        json!({
            "project": "agent:eval:label-safety",
            "purpose": "test",
            "command": "legacy validation",
            "assertion_name": unsafe_legacy,
        }),
        false,
        json!({
            "exit_code": 1,
            "purpose": "test",
            "execution_state": "completed",
        }),
    );

    let validation =
        validation_summary_for_session(&store.summary(&session.session_id, Some(50)).unwrap());
    let events = validation["events"].as_array().unwrap();
    let structured = events
        .iter()
        .find(|event| event["tool_name"] == "cargo_check")
        .unwrap();
    let generic = events
        .iter()
        .find(|event| event["tool_name"] == "run_process")
        .unwrap();
    assert!(structured.get("assertion_name").is_none());
    assert!(generic.get("assertion_name").is_none());
    let serialized = validation.to_string();
    assert!(!serialized.contains(hidden_structured));
    assert!(!serialized.contains(unsafe_legacy));
}

#[test]
fn materialized_run_script_terminal_preserves_recoverable_assertion_label() {
    let store = SessionStore::default();
    let project = "agent:eval:script-terminal";
    let session = store.start_session(Some(project.to_string()), None);
    let assertion_name = "promoted script validation";
    let identity = assertion_validation_identity(assertion_name);
    let terminal_output = json!({
        "exit_code": 0,
        "purpose": "test",
        "execution_state": "completed",
        "language": "bash",
        "stdout_tail": "script validation passed\n",
        "stderr_tail": "",
        "stdout_truncated": false,
        "stderr_truncated": false,
    });
    let terminal_summary =
        sessions::execution_output_summary_for_tool_result("run_script", &terminal_output);
    assert!(store.record_validation_job_terminal(
        &session.session_id,
        "job_script_assertion",
        &["job_script_assertion"],
        "run_script",
        session_tool_contract("run_script"),
        Some(project.to_string()),
        &identity,
        Some(assertion_name),
        "completed",
        Some(0),
        Some(true),
        Some(100),
        Some(110),
        Some(10_000),
        terminal_summary,
    ));

    let validation =
        validation_summary_for_session(&store.summary(&session.session_id, Some(20)).unwrap());
    let latest = &validation["latest"];
    assert_eq!(latest["execution_source"], "run_script");
    assert_eq!(latest["identity"], identity);
    assert_eq!(latest["assertion_name"], assertion_name);
}

#[test]
fn successful_validation_followed_by_failure_marks_historical_failure_unresolved() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_check",
        json!({"project": "agent:eval:demo"}),
        true,
        json!({"exit_code": 0}),
    );
    record_finished_tool(
        &store,
        &session.session_id,
        "cargo_test",
        json!({"project": "agent:eval:demo"}),
        false,
        json!({"exit_code": 101}),
    );

    let session = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&session);

    assert_eq!(validation["available"], true);
    assert_eq!(validation["status"], "mixed");
    assert_eq!(validation["latest_status"], "failed");
    assert_eq!(validation["historical_failures"]["count"], 1);
    assert_eq!(validation["historical_failures"]["resolved"], false);
    assert_eq!(validation["historical_failures"]["unresolved"], true);
    assert_eq!(validation["latest"]["tool_name"], "cargo_test");
    assert_eq!(validation["latest"]["success"], false);
    assert_eq!(validation["latest_success"]["tool_name"], "cargo_check");
    assert_eq!(validation["latest_failure"]["tool_name"], "cargo_test");
}

#[test]
fn current_evidence_failure_then_content_change_then_different_success_passes() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_validation_failure(&store, &session.session_id, "cargo_test");
    record_content_mutation(&store, &session.session_id, true);
    record_validation_success(&store, &session.session_id, "cargo_check");

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["status"], "mixed");
    assert_eq!(validation["historical_failures"]["count"], 1);
    assert_eq!(validation["unresolved_failures"]["count"], 1);
    assert_eq!(validation["current_evidence"]["status"], "passed");
    assert_eq!(
        validation["current_evidence"]["unresolved_failure_count"],
        0
    );
    assert_eq!(validation["current_evidence"]["stale_failure_count"], 1);
    assert_eq!(
        validation["current_evidence"]["boundary_reason"],
        "workspace_content_changed"
    );
}

#[test]
fn current_evidence_validation_started_before_content_change_then_pass_is_stale() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    let validation_start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "cargo_check",
        &json!({"project": "agent:eval:demo"}),
        session_tool_contract("cargo_check"),
    );
    record_content_mutation(&store, &session.session_id, true);
    store.record_tool_call_finished(validation_start, true, &json!({"exit_code": 0}), None, None);

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["status"], "passed");
    assert_eq!(validation["current_evidence"]["status"], "stale");
    assert_eq!(validation["current_evidence"]["events_total"], 0);
    assert_eq!(
        validation["current_evidence"]["evidence_after_latest_content_change"],
        false
    );
}

#[test]
fn current_evidence_validation_started_before_content_change_then_fail_is_stale() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    let validation_start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "cargo_test",
        &json!({"project": "agent:eval:demo"}),
        session_tool_contract("cargo_test"),
    );
    record_content_mutation(&store, &session.session_id, true);
    store.record_tool_call_finished(
        validation_start,
        false,
        &json!({"exit_code": 101, "failure_kind": "validation_failed"}),
        Some("validation failed"),
        None,
    );

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["status"], "failed");
    assert_eq!(validation["unresolved_failures"]["count"], 1);
    assert_eq!(validation["current_evidence"]["status"], "stale");
    assert_eq!(
        validation["current_evidence"]["unresolved_failure_count"],
        0
    );
    assert_eq!(validation["current_evidence"]["stale_failure_count"], 1);
}

#[test]
fn current_evidence_validation_started_after_content_change_can_pass() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_content_mutation(&store, &session.session_id, true);
    record_validation_success(&store, &session.session_id, "cargo_check");

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["current_evidence"]["status"], "passed");
    assert_eq!(validation["current_evidence"]["events_total"], 1);
    assert_eq!(
        validation["current_evidence"]["evidence_after_latest_content_change"],
        true
    );
}

#[test]
fn current_evidence_validation_started_before_new_attempt_does_not_enter_new_attempt() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    let validation_start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "cargo_check",
        &json!({"project": "agent:eval:demo"}),
        session_tool_contract("cargo_check"),
    );
    record_task_instruction(&store, &session.session_id, "review current workspace");
    store.record_tool_call_finished(validation_start, true, &json!({"exit_code": 0}), None, None);

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["status"], "passed");
    assert_eq!(validation["current_evidence"]["status"], "not_run");
    assert_eq!(validation["current_evidence"]["events_total"], 0);
}

#[test]
fn validation_job_terminal_inherits_public_failure_expectation() {
    let store = SessionStore::default();
    let project = "agent:eval:demo";
    let session = store.start_session(Some(project.to_string()), None);
    let job_id = "job_expected_negative_validation";
    let target = "target:aaaaaaaaaaaaaaaaaaaaaaaa";

    let start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "cargo_check",
        &json!({
            "project": project,
            "result_expectation": "failure"
        }),
        session_tool_contract("cargo_check"),
    );
    store.record_tool_call_finished(
        start,
        true,
        &json!({
            "job_id": job_id,
            "job_status": "running",
            "promoted_to_job": true,
            "execution_state": "running",
            "command_started": true,
            "command_completed": false,
            "stdout_tail": "",
            "stderr_tail": "",
            "stdout_lines": 0,
            "stderr_lines": 0,
            "stdout_truncated": false,
            "stderr_truncated": false
        }),
        None,
        None,
    );

    assert!(store.record_validation_job_terminal(
        &session.session_id,
        job_id,
        &[job_id],
        "cargo_check",
        session_tool_contract("cargo_check"),
        Some(project.to_string()),
        target,
        None,
        "failed",
        Some(101),
        Some(false),
        Some(100),
        Some(110),
        Some(10_000),
        None,
    ));

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let terminal = summary
        .events
        .iter()
        .find(|event| event.kind == "validation_job_terminal")
        .expect("materialized terminal validation event");
    assert_eq!(terminal.status.as_deref(), Some("failed"));
    assert_eq!(terminal.exit_code, Some(101));
    assert_eq!(terminal.result_expectation.as_deref(), Some("failure"));
    assert_eq!(
        terminal.failure_expectation_result.as_deref(),
        Some("matched_expected_failure")
    );

    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["status"], "expected");
    assert_eq!(validation["latest_status"], "expected");
    assert_eq!(validation["latest"]["success"], false);
    assert_eq!(validation["latest"]["execution_success"], false);
    assert_eq!(validation["latest"]["expectation_satisfied"], true);
    assert_eq!(validation["expected_results"], 1);
    assert_eq!(validation["latest"]["exit_code"], 101);
    assert_eq!(validation["unresolved_failures"]["count"], 0);
}

#[test]
fn validation_job_terminal_inherits_expectation_beyond_default_summary_window() {
    let store = SessionStore::default();
    let project = "agent:eval:demo";
    let session = store.start_session(Some(project.to_string()), None);
    let job_id = "job_expected_negative_validation_deep_history";
    let target = "target:bbbbbbbbbbbbbbbbbbbbbbbb";

    let start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "cargo_check",
        &json!({
            "project": project,
            "result_expectation": "failure"
        }),
        session_tool_contract("cargo_check"),
    );
    store.record_tool_call_finished(
        start,
        true,
        &json!({
            "job_id": job_id,
            "job_status": "running",
            "promoted_to_job": true,
            "execution_state": "running",
            "command_started": true,
            "command_completed": false,
            "stdout_tail": "",
            "stderr_tail": "",
            "stdout_lines": 0,
            "stderr_lines": 0,
            "stdout_truncated": false,
            "stderr_truncated": false
        }),
        None,
        None,
    );

    // Keep the Job admission inside the retained 200-event ledger while
    // pushing it outside the public/default 50-event Session summary window.
    for index in 0..30 {
        record_finished_tool(
            &store,
            &session.session_id,
            "read_file",
            json!({"project": project, "path": format!("src/noise-{index}.rs")}),
            true,
            json!({}),
        );
    }

    assert!(store.record_validation_job_terminal(
        &session.session_id,
        job_id,
        &[job_id],
        "cargo_check",
        session_tool_contract("cargo_check"),
        Some(project.to_string()),
        target,
        None,
        "failed",
        Some(101),
        Some(false),
        Some(100),
        Some(110),
        Some(10_000),
        None,
    ));

    let summary = store.summary(&session.session_id, Some(200)).unwrap();
    let terminal = summary
        .events
        .iter()
        .find(|event| event.kind == "validation_job_terminal")
        .expect("materialized terminal validation event");
    assert_eq!(terminal.result_expectation.as_deref(), Some("failure"));
    assert_eq!(
        terminal.failure_expectation_result.as_deref(),
        Some("matched_expected_failure")
    );
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["status"], "expected");
    assert_eq!(validation["latest_status"], "expected");
    assert_eq!(validation["latest"]["success"], false);
    assert_eq!(validation["latest"]["execution_success"], false);
    assert_eq!(validation["latest"]["expectation_satisfied"], true);
    assert_eq!(validation["expected_results"], 1);
    assert_eq!(validation["unresolved_failures"]["count"], 0);
}

#[test]
fn expected_observation_failure_does_not_resolve_prior_same_identity_validation_failure() {
    let store = SessionStore::default();
    let project = "agent:eval:demo";
    let session = store.start_session(Some(project.to_string()), None);
    let assertion_name = "same regression remains failing";
    let failed_output = json!({
        "exit_code": 1,
        "failure_kind": "command_exit_nonzero",
        "command_started": true,
        "command_completed": true,
        "execution_state": "completed"
    });

    record_finished_tool(
        &store,
        &session.session_id,
        "run_process",
        json!({
            "project": project,
            "purpose": "test",
            "assertion_name": assertion_name
        }),
        false,
        failed_output.clone(),
    );
    record_finished_tool(
        &store,
        &session.session_id,
        "run_process",
        json!({
            "project": project,
            "purpose": "test",
            "assertion_name": assertion_name,
            "result_expectation": "observe"
        }),
        false,
        failed_output,
    );

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["events_total"], 2);
    assert_eq!(validation["successes"], 0);
    assert_eq!(validation["failures"], 1);
    assert_eq!(validation["expected_results"], 1);
    assert_eq!(validation["status"], "failed");
    assert_eq!(validation["latest_status"], "expected");
    assert_eq!(validation["historical_failures"]["count"], 1);
    assert_eq!(validation["historical_failures"]["resolved"], false);
    assert_eq!(validation["historical_failures"]["unresolved"], true);
    assert_eq!(validation["resolved_failures"]["count"], 0);
    assert_eq!(validation["unresolved_failures"]["count"], 1);
    assert_eq!(validation["current_evidence"]["status"], "failed");
    assert_eq!(
        validation["current_evidence"]["unresolved_failure_count"],
        1
    );
    assert_eq!(validation["latest"]["success"], false);
    assert_eq!(validation["latest"]["execution_success"], false);
    assert_eq!(validation["latest"]["expectation_satisfied"], true);
}

#[test]
fn current_evidence_validation_job_started_before_content_change_is_stale() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    let job_id = "job_current_evidence_overlap";
    let validation_start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "cargo_check",
        &json!({"project": "agent:eval:demo"}),
        session_tool_contract("cargo_check"),
    );
    store.record_tool_call_finished(
        validation_start,
        true,
        &json!({"job_id": job_id, "execution_state": "started"}),
        None,
        None,
    );
    record_content_mutation(&store, &session.session_id, true);
    assert!(store.record_validation_job_terminal(
        &session.session_id,
        job_id,
        &[job_id],
        "cargo_check",
        session_tool_contract("cargo_check"),
        Some("agent:eval:demo".to_string()),
        "target:aaaaaaaaaaaaaaaaaaaaaaaa",
        None,
        "completed",
        Some(0),
        Some(true),
        Some(100),
        Some(110),
        Some(10_000),
        None,
    ));

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["status"], "passed");
    assert_eq!(validation["current_evidence"]["status"], "stale");
    assert_eq!(validation["current_evidence"]["events_total"], 0);
}

#[test]
fn current_evidence_legacy_validation_without_exact_start_correlation_is_not_current() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_validation_success(&store, &session.session_id, "cargo_check");

    let mut summary = store.summary(&session.session_id, Some(50)).unwrap();
    for event in &mut summary.events {
        if event.tool_name == "cargo_check" {
            event.call_id = None;
        }
    }
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["status"], "passed");
    assert_eq!(validation["current_evidence"]["status"], "not_run");
    assert_eq!(validation["current_evidence"]["events_total"], 0);
}

#[test]
fn current_evidence_different_success_without_mutation_keeps_failure_open() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_validation_failure(&store, &session.session_id, "cargo_test");
    record_validation_success(&store, &session.session_id, "cargo_check");

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["current_evidence"]["status"], "failed");
    assert_eq!(
        validation["current_evidence"]["unresolved_failure_count"],
        1
    );
    assert_eq!(validation["current_evidence"]["stale_failure_count"], 0);
    assert_eq!(
        validation["current_evidence"]["boundary_reason"],
        "attempt_start"
    );
}

#[test]
fn current_evidence_same_identity_failure_success_resolves_inside_window() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_validation_failure(&store, &session.session_id, "cargo_check");
    record_validation_success(&store, &session.session_id, "cargo_check");

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["status"], "mixed");
    assert_eq!(validation["resolved_failures"]["count"], 1);
    assert_eq!(validation["unresolved_failures"]["count"], 0);
    assert_eq!(validation["current_evidence"]["status"], "passed");
    assert_eq!(validation["current_evidence"]["resolved_failure_count"], 1);
    assert_eq!(
        validation["current_evidence"]["unresolved_failure_count"],
        0
    );
}

#[test]
fn current_evidence_pass_then_content_change_is_stale() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_validation_success(&store, &session.session_id, "cargo_check");
    record_content_mutation(&store, &session.session_id, true);

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["status"], "passed");
    assert_eq!(validation["current_evidence"]["status"], "stale");
    assert_eq!(validation["current_evidence"]["events_total"], 0);
    assert_eq!(
        validation["current_evidence"]["evidence_after_latest_content_change"],
        false
    );
}

#[test]
fn current_evidence_failure_then_content_change_without_validation_is_stale_not_failed() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_validation_failure(&store, &session.session_id, "cargo_test");
    record_content_mutation(&store, &session.session_id, true);

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["status"], "failed");
    assert_eq!(validation["unresolved_failures"]["count"], 1);
    assert_eq!(validation["current_evidence"]["status"], "stale");
    assert_eq!(
        validation["current_evidence"]["unresolved_failure_count"],
        0
    );
    assert_eq!(validation["current_evidence"]["stale_failure_count"], 1);
}

#[test]
fn current_evidence_failed_noop_mutation_does_not_reset() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_validation_failure(&store, &session.session_id, "cargo_test");
    record_finished_tool(
        &store,
        &session.session_id,
        "apply_text_edits",
        json!({"project": "agent:eval:demo", "changes": [{"kind": "edit", "path": "src/lib.rs"}]}),
        false,
        json!({"state_changed": false, "failure_kind": "stale_precondition"}),
    );
    record_validation_success(&store, &session.session_id, "cargo_check");

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["current_evidence"]["status"], "failed");
    assert_eq!(
        validation["current_evidence"]["unresolved_failure_count"],
        1
    );
}

#[test]
fn current_evidence_metadata_only_state_change_does_not_reset() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_validation_failure(&store, &session.session_id, "cargo_test");
    record_finished_tool(
        &store,
        &session.session_id,
        "git_push",
        json!({"project": "agent:eval:demo"}),
        true,
        json!({"state_changed": true}),
    );
    record_validation_success(&store, &session.session_id, "cargo_check");

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["current_evidence"]["status"], "failed");
    assert_eq!(
        validation["current_evidence"]["unresolved_failure_count"],
        1
    );
    assert_eq!(
        validation["current_evidence"]["boundary_reason"],
        "attempt_start"
    );
}

#[test]
fn current_evidence_second_content_change_resets_post_first_change_validation() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_validation_failure(&store, &session.session_id, "cargo_test");
    record_content_mutation(&store, &session.session_id, true);
    record_validation_success(&store, &session.session_id, "cargo_check");
    record_content_mutation(&store, &session.session_id, true);

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["current_evidence"]["status"], "stale");
    assert_eq!(validation["current_evidence"]["events_total"], 0);
    assert_eq!(
        validation["current_evidence"]["unresolved_failure_count"],
        0
    );
}

#[test]
fn current_evidence_old_attempt_failure_is_excluded() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_validation_failure(&store, &session.session_id, "cargo_test");
    record_task_instruction(&store, &session.session_id, "review current workspace");
    record_finished_tool(
        &store,
        &session.session_id,
        "read_file",
        json!({"project": "agent:eval:demo", "path": "src/lib.rs"}),
        true,
        json!({}),
    );

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["status"], "failed");
    assert_eq!(validation["unresolved_failures"]["count"], 1);
    assert_eq!(validation["current_evidence"]["status"], "not_run");
    assert_eq!(
        validation["current_evidence"]["unresolved_failure_count"],
        0
    );
}

#[test]
fn current_evidence_legacy_mutation_without_effect_proof_is_conservative() {
    let store = SessionStore::default();
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    record_validation_failure(&store, &session.session_id, "cargo_test");
    record_content_mutation(&store, &session.session_id, false);
    record_validation_success(&store, &session.session_id, "cargo_check");

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let validation = validation_summary_for_session(&summary);
    assert_eq!(validation["current_evidence"]["status"], "failed");
    assert_eq!(
        validation["current_evidence"]["unresolved_failure_count"],
        1
    );
    assert_eq!(
        validation["current_evidence"]["boundary_reason"],
        "attempt_start"
    );
}

fn record_validation_failure(store: &SessionStore, session_id: &str, tool_name: &str) {
    record_finished_tool(
        store,
        session_id,
        tool_name,
        json!({"project": "agent:eval:demo"}),
        false,
        json!({"exit_code": 101, "failure_kind": "validation_failed"}),
    );
}

fn record_validation_success(store: &SessionStore, session_id: &str, tool_name: &str) {
    record_finished_tool(
        store,
        session_id,
        tool_name,
        json!({"project": "agent:eval:demo"}),
        true,
        json!({"exit_code": 0}),
    );
}

fn record_content_mutation(store: &SessionStore, session_id: &str, proven: bool) {
    record_finished_tool(
        store,
        session_id,
        "apply_text_edits",
        json!({"project": "agent:eval:demo", "changes": [{"kind": "edit", "path": "src/lib.rs"}]}),
        true,
        if proven {
            json!({"state_changed": true})
        } else {
            json!({})
        },
    );
}

fn record_task_instruction(store: &SessionStore, session_id: &str, instruction: &str) {
    store
        .ensure_coding_session(sessions::CodingSessionRequest {
            project: "agent:eval:demo".to_string(),
            authority_fingerprint: sessions::TEST_ONLY_PROJECT_SESSION_AUTHORITY_FINGERPRINT
                .to_string(),
            resume_session_id: Some(session_id.to_string()),
            instruction: Some(instruction.to_string()),
            mode: SessionMode::Normal,
            guards: SessionGuards::default(),
            execution_context: None,
            project_instructions: None,
            transport: SessionTransport::Api,
            context_refreshed: true,
            write_scope_verified: true,
        })
        .unwrap();
}

fn record_finished_tool(
    store: &SessionStore,
    session_id: &str,
    tool_name: &str,
    arguments: Value,
    success: bool,
    output: Value,
) {
    let start = store.record_tool_call_started(
        Some(session_id),
        SessionTransport::Api,
        tool_name,
        &arguments,
        session_tool_contract(tool_name),
    );
    let error = (!success).then_some("tool failed");
    store.record_tool_call_finished(start, success, &output, error, None);
}

fn json_contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key(key) || map.values().any(|value| json_contains_key(value, key))
        }
        Value::Array(values) => values.iter().any(|value| json_contains_key(value, key)),
        _ => false,
    }
}

fn assert_no_raw_validation_output_fields(value: &Value, context: &str) {
    for key in [
        "stdout",
        "stderr",
        "stdout_tail",
        "stderr_tail",
        "stdout_tail_excerpt",
        "stderr_tail_excerpt",
        "validation_output_summary",
    ] {
        assert!(
            !json_contains_key(value, key),
            "{context} must not include {key}: {value}"
        );
    }
}
