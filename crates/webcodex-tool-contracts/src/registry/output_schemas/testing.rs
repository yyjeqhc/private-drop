use serde_json::{json, Value};

use super::common::{
    cargo_test_count_assertion_schema, job_activity_schema, nullable_schema,
    observe_job_continuation_schema, open_object_schema, permission_decision_schema, schema_type,
    session_hint_schema,
};

pub(super) fn output_schema_for_tool(name: &str) -> Option<Value> {
    match name {
        "cargo_fmt" | "cargo_check" | "cargo_test" | "go_test" => Some(cargo_output_schema(name)),
        _ => None,
    }
}

fn cargo_output_schema(tool_name: &str) -> Value {
    let mut fields = vec![
            ("project", schema_type("string", "Runtime project id.")),
            ("command_summary", schema_type("string", "Bounded structured validation command summary.")),
            ("shell", schema_type("string", "Executor command mode.")),
            ("executor", json!({
                "type": "string",
                "const": "agent",
                "description": "Runner-backed structured validation executor."
            })),
            (
                "cwd",
                schema_type("string", "Project-relative working directory."),
            ),
            (
                "exit_code",
                nullable_schema("integer", "Validation command exit code."),
            ),
            (
                "duration_ms",
                nullable_schema("integer", "Command duration in milliseconds."),
            ),
            ("stdout_tail", schema_type("string", "Bounded stdout tail.")),
            ("stderr_tail", schema_type("string", "Bounded stderr tail.")),
            ("stdout_lines", schema_type("integer", "Observed stdout line count.")),
            ("stderr_lines", schema_type("integer", "Observed stderr line count.")),
            (
                "stdout_truncated",
                schema_type("boolean", "Whether stdout was truncated."),
            ),
            (
                "stderr_truncated",
                schema_type("boolean", "Whether stderr was truncated."),
            ),
            (
                "passed",
                nullable_schema("boolean", "Whether command execution succeeded and every requested structured validation postcondition was proven. Absent while a promoted Job is still running."),
            ),
            (
                "command_started",
                schema_type("boolean", "Whether callers must conservatively treat the command as started; true includes outcome_unknown because side effects may have occurred."),
            ),
            (
                "command_completed",
                schema_type("boolean", "Whether the command reached a trustworthy terminal executor result. False for outcome_unknown, timed_out, and Job handoff."),
            ),
            (
                "failure_kind",
                schema_type(
                    "string",
                    "Stable failure kind. Non-zero structured validation exits and an unmet/unproven Cargo test-count assertion use validation_failed; outcome_unknown, pre-start rejection, guard denial, timeout, and runtime errors remain distinct.",
                ),
            ),
            (
                "warnings_count",
                nullable_schema("integer", "Heuristic warning count for cargo_check."),
            ),
            (
                "errors_count",
                nullable_schema("integer", "Heuristic error count for cargo_check."),
            ),
            (
                "tests_passed",
                nullable_schema("integer", "Parsed passed test count for structured test validation."),
            ),
            (
                "tests_failed",
                nullable_schema("integer", "Parsed failed test count for structured test validation."),
            ),
            (
                "execution_source",
                schema_type("string", "Structured validation tool that owns this execution."),
            ),
            (
                "purpose",
                schema_type("string", "Declared execution purpose (test, format, or validation)."),
            ),
            (
                "execution_state",
                schema_type("string", "not_started, outcome_unknown, completed, timed_out, queued, or running. not_started proves pre-execution rejection; outcome_unknown means side effects may have occurred and blind retry is unsafe; queued/running indicate promotion to a Job."),
            ),
            (
                "job_id",
                nullable_schema("string", "Runtime job id when the validation continues as a Job."),
            ),
            (
                "job_status",
                nullable_schema("string", "Job status when the validation continues as a Job."),
            ),
            (
                "observation_token",
                json!({
                    "type": "string",
                    "maxLength": webcodex_core::job_observation::MAX_JOB_OBSERVATION_TOKEN_LEN,
                    "description": "Current opaque observation token for the exact public Job snapshot returned by a promoted validation handoff."
                }),
            ),
            ("activity", job_activity_schema()),
            (
                "promoted_to_job",
                schema_type("boolean", "True only when the validation was promoted to a Job and the same command continues running."),
            ),
            (
                "effective_timeout_secs",
                schema_type("integer", "Effective total runtime budget of the command in seconds."),
            ),
            (
                "sync_wait_secs",
                schema_type("integer", "Internal synchronous wait window before promotion, in seconds."),
            ),
            (
                "terminal",
                schema_type("boolean", "True when the execution request has a known terminal projection, including not_started or timed_out. False for Job handoff and outcome_unknown."),
            ),
            ("command_ok", schema_type("boolean", "Whether execution completed successfully.")),
            ("tool_failure", schema_type("boolean", "Whether rejection happened before execution.")),
            ("async_handoff_available", schema_type("boolean", "Whether this Runner supports validation Job handoff.")),
            ("detected_summary", open_object_schema("Current bounded validation/progress summary at the initial durable Job handoff; advisory only and never retry authority.")),
            ("continuation", observe_job_continuation_schema()),
            ("session_hint", session_hint_schema()),
            ("permission", permission_decision_schema()),
    ];
    if tool_name == "cargo_fmt" {
        fields.extend([
            (
                "changed",
                nullable_schema(
                    "boolean",
                    "For check=false ensure-format: true when precheck proved formatting was needed and the mutating cargo fmt completed successfully, false when no mutation was needed or mutation definitely did not start, null when mutation may have changed source but the final effect is uncertain. Absent for check=true validation.",
                ),
            ),
            (
                "state_changed",
                nullable_schema(
                    "boolean",
                    "Effect-state projection for check=false ensure-format. Mirrors changed on known outcomes and is null when post-dispatch mutation state is uncertain. Absent for check=true validation.",
                ),
            ),
        ]);
    }
    if matches!(tool_name, "cargo_check" | "cargo_test" | "go_test") {
        fields.push((
            "diagnostics",
            cargo_test_diagnostics_schema(
                "Deterministic structured validation evidence extracted from bounded validation output.",
            ),
        ));
    }
    if matches!(tool_name, "cargo_test" | "go_test") {
        fields.extend([
            (
                "tests_detected",
                schema_type(
                    "boolean",
                    "Whether structured test output proved at least one test result/count section.",
                ),
            ),
            (
                "tests_run_count",
                nullable_schema(
                    "integer",
                    "Total tests represented by the structured test evidence.",
                ),
            ),
            (
                "zero_tests_run",
                nullable_schema(
                    "boolean",
                    "True when structured test evidence is available and its tests_run_count is zero.",
                ),
            ),
        ]);
    }
    if tool_name == "cargo_test" {
        fields.extend([
            (
                "require_tests",
                schema_type(
                    "boolean",
                    "Echo of an explicitly supplied require_tests policy. false explicitly accepts zero executed tests as proof when no min_tests assertion is present; omission retains the default requirement for non-zero executed-test proof.",
                ),
            ),
            (
                "no_run",
                schema_type(
                    "boolean",
                    "Echo of an explicitly supplied no_run policy. true is compile-only validation and does not require executed-test count proof.",
                ),
            ),
            ("test_count_assertion", cargo_test_count_assertion_schema()),
        ]);
    }
    let properties = fields
        .into_iter()
        .map(|(name, schema)| (name.to_string(), schema))
        .collect::<serde_json::Map<_, _>>();
    let mut terminal_required = vec![
        "project",
        "command_summary",
        "cwd",
        "shell",
        "executor",
        "execution_source",
        "purpose",
        "execution_state",
        "exit_code",
        "duration_ms",
        "stdout_tail",
        "stderr_tail",
        "stdout_lines",
        "stderr_lines",
        "stdout_truncated",
        "stderr_truncated",
        "promoted_to_job",
        "terminal",
        "command_started",
        "command_completed",
        "passed",
        "effective_timeout_secs",
        "sync_wait_secs",
    ];
    match tool_name {
        "cargo_check" => {
            terminal_required.extend(["warnings_count", "errors_count", "diagnostics"])
        }
        "cargo_test" | "go_test" => terminal_required.extend([
            "tests_detected",
            "tests_run_count",
            "tests_passed",
            "tests_failed",
            "zero_tests_run",
            "diagnostics",
        ]),
        _ => {}
    }
    let terminal_required = terminal_required
        .into_iter()
        .map(Value::from)
        .collect::<Vec<_>>();
    let mut terminal_failure_required = terminal_required.clone();
    terminal_failure_required.push(Value::from("failure_kind"));
    let output = json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false
    });
    json!({
        "type": "object",
        "properties": {
            "success": { "type": "boolean" },
            "output": output.clone(),
            "error": { "anyOf": [{"type": "string"}, {"type": "null"}] }
        },
        "required": ["success"],
        "allOf": [
            {
                "type": "object",
                "properties": {
                    "success": { "type": "boolean" },
                    "output": output,
                    "error": { "anyOf": [{"type": "string"}, {"type": "null"}] }
                },
                "required": ["success", "output"],
                "additionalProperties": false
            },
            {
                "oneOf": [
            {
                "properties": {
                    "success": {"const": true},
                    "error": {"type": "null"},
                    "output": {
                        "required": [
                            "project", "command_summary", "cwd", "shell", "executor",
                            "execution_source", "purpose", "promoted_to_job", "terminal",
                            "command_started", "command_completed", "execution_state", "job_id",
                            "job_status", "observation_token", "activity", "continuation", "effective_timeout_secs", "sync_wait_secs",
                            "stdout_tail", "stderr_tail", "stdout_lines", "stderr_lines",
                            "stdout_truncated", "stderr_truncated"
                        ],
                        "properties": {
                            "promoted_to_job": {"const": true},
                            "terminal": {"const": false},
                            "command_completed": {"const": false},
                            "execution_state": {"enum": ["queued", "running"]},
                            "job_id": {"type": "string", "minLength": 1},
                            "job_status": {"type": "string", "minLength": 1},
                            "observation_token": {"type": "string", "minLength": 1, "maxLength": webcodex_core::job_observation::MAX_JOB_OBSERVATION_TOKEN_LEN},
                            "passed": {"enum": []},
                            "failure_kind": {"enum": []},
                            "warnings_count": {"enum": []},
                            "errors_count": {"enum": []},
                            "tests_detected": {"enum": []},
                            "tests_run_count": {"enum": []},
                            "tests_passed": {"enum": []},
                            "tests_failed": {"enum": []},
                            "zero_tests_run": {"enum": []},
                            "diagnostics": {"enum": []}
                        }
                    }
                }
            },
            {
                "properties": {
                    "success": {"const": true},
                    "error": {"type": "null"},
                    "output": {
                        "required": terminal_required.clone(),
                        "properties": {
                            "promoted_to_job": {"const": false},
                            "terminal": {"const": true},
                            "command_started": {"const": true},
                            "command_completed": {"const": true},
                            "execution_state": {"const": "completed"},
                            "passed": {"const": true},
                            "failure_kind": {"enum": []},
                            "job_id": {"enum": []},
                            "job_status": {"enum": []},
                            "observation_token": {"enum": []},
                            "continuation": {"enum": []}
                        }
                    }
                }
            },
            {
                "required": ["error"],
                "properties": {
                    "success": {"const": false},
                    "error": {"type": "string", "minLength": 1},
                    "output": {
                        "required": terminal_failure_required.clone(),
                        "properties": {
                            "promoted_to_job": {"const": false},
                            "terminal": {"const": true},
                            "command_started": {"const": true},
                            "command_completed": {"const": false},
                            "execution_state": {"const": "timed_out"},
                            "passed": {"const": false},
                            "failure_kind": {"const": "timeout"},
                            "job_id": {"enum": []},
                            "job_status": {"enum": []},
                            "observation_token": {"enum": []},
                            "continuation": {"enum": []}
                        }
                    }
                }
            },
            {
                "required": ["error"],
                "properties": {
                    "success": {"const": false},
                    "error": {"type": "string", "minLength": 1},
                    "output": {
                        "required": terminal_failure_required.clone(),
                        "properties": {
                            "promoted_to_job": {"const": false},
                            "terminal": {"const": false},
                            "command_started": {"const": true},
                            "command_completed": {"const": false},
                            "execution_state": {"const": "outcome_unknown"},
                            "passed": {"const": false},
                            "failure_kind": {"const": "outcome_unknown"},
                            "job_id": {"enum": []},
                            "job_status": {"enum": []},
                            "observation_token": {"enum": []},
                            "continuation": {"enum": []}
                        }
                    }
                }
            },
            {
                "required": ["error"],
                "properties": {
                    "success": {"const": false},
                    "error": {"type": "string", "minLength": 1},
                    "output": {
                        "required": terminal_failure_required.clone(),
                        "properties": {
                            "promoted_to_job": {"const": false},
                            "terminal": {"const": true},
                            "command_started": {"const": false},
                            "command_completed": {"const": false},
                            "execution_state": {"const": "not_started"},
                            "passed": {"const": false},
                            "failure_kind": {"enum": ["permission_denied", "project_not_found", "cwd_invalid", "sandbox_unavailable", "executor_unavailable"]},
                            "job_id": {"enum": []},
                            "job_status": {"enum": []},
                            "observation_token": {"enum": []},
                            "continuation": {"enum": []}
                        }
                    }
                }
            },
            {
                "required": ["error"],
                "properties": {
                    "success": {"const": false},
                    "error": {"type": "string", "minLength": 1},
                    "output": {
                        "required": terminal_failure_required,
                        "properties": {
                            "promoted_to_job": {"const": false},
                            "terminal": {"const": true},
                            "command_started": {"const": true},
                            "command_completed": {"const": true},
                            "execution_state": {"const": "completed"},
                            "passed": {"const": false},
                            "failure_kind": {"enum": ["validation_failed", "process_exit"]},
                            "job_id": {"enum": []},
                            "job_status": {"enum": []},
                            "observation_token": {"enum": []},
                            "continuation": {"enum": []}
                        }
                    }
                }
            },
            {
                "required": ["error"],
                "properties": {
                    "success": {"const": false},
                    "error": {"type": "string", "minLength": 1},
                    "output": {
                        "required": ["execution_source", "command_started", "command_completed", "failure_kind"],
                        "properties": {
                            "command_started": {"const": false},
                            "command_completed": {"const": false},
                            "failure_kind": {"enum": ["invalid_arguments", "capability_unavailable", "permission_denied", "project_not_found", "cwd_invalid", "sandbox_unavailable", "executor_unavailable"]},
                            "job_id": {"enum": []},
                            "job_status": {"enum": []},
                            "observation_token": {"enum": []},
                            "continuation": {"enum": []},
                            "passed": {"enum": []},
                            "promoted_to_job": {"enum": []},
                            "terminal": {"enum": []}
                        }
                    }
                }
            }
                ]
            }
        ]
    })
}

fn cargo_test_diagnostics_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "properties": {
            "available": schema_type(
                "boolean",
                "True when a test result summary or at least one safe failed test name was parsed.",
            ),
            "parser": {
                "type": "string",
                "enum": ["structured_validation_parser"],
                "description": "Stable structured validation parser v3 identifier."
            },
            "reason": nullable_schema(
                "string",
                "Why diagnostics are unavailable, when available is false.",
            ),
            "diagnostic_count": nullable_schema(
                "integer",
                "Prefer test_summary.failed; otherwise the number of safely captured failed test names.",
            ),
            "diagnostics": {
                "type": "array",
                "maxItems": 20,
                "items": cargo_diagnostic_schema(),
                "description": "Bounded sorted, deduplicated compiler diagnostics when the validation parser provides them."
            },
            "returned_diagnostic_count": {
                "type": "integer",
                "minimum": 0,
                "maximum": 20
            },
            "diagnostics_truncated": schema_type(
                "boolean",
                "True when the diagnostic list or captured validation excerpt is incomplete.",
            ),
            "invalid_diagnostics_omitted": {
                "type": "integer",
                "minimum": 0,
                "description": "Diagnostic headers omitted because they could not be represented safely."
            },
            "test_summary": {
                "type": "object",
                "description": "Aggregated structured test result summary counts from bounded validation evidence.",
                "properties": {
                    "passed": nullable_schema("integer", "Aggregated passed test count."),
                    "failed": nullable_schema("integer", "Aggregated failed test count."),
                    "ignored": nullable_schema("integer", "Aggregated ignored test count."),
                },
                "additionalProperties": false,
            },
            "failed_test_details": {
                "type": "array",
                "maxItems": 20,
                "items": failed_test_detail_schema(),
                "description": "Up to 20 unique failed-test details in deterministic first-seen order. Conservative assertion, panic, or unknown evidence without payload bodies."
            },
            "failed_test_details_truncated": schema_type(
                "boolean",
                "True when more than 20 unique safe names were seen, or the bounded excerpt was truncated and the aggregated summary failed count exceeds captured details.",
            ),
            "truncated": schema_type(
                "boolean",
                "Whether the parsed stdout_tail and/or stderr_tail input was truncated.",
            ),
        },
        "additionalProperties": false,
    })
}

fn cargo_diagnostic_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "severity": { "type": "string", "enum": ["error", "warning", "unknown"] },
            "code": { "type": "string", "maxLength": 64 },
            "file": { "type": "string", "maxLength": 512 },
            "line": { "type": "integer", "minimum": 1 },
            "column": { "type": "integer", "minimum": 1 },
            "message": { "type": "string", "maxLength": 240 }
        },
        "required": ["severity", "message"]
    })
}

fn failed_test_detail_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "name": { "type": "string", "maxLength": 240 },
            "failure_kind": { "type": "string", "enum": ["assertion", "panic", "unknown"] },
            "file": { "anyOf": [{"type": "string", "maxLength": 512}, {"type": "null"}] },
            "line": { "anyOf": [{"type": "integer", "minimum": 1}, {"type": "null"}] },
            "column": { "anyOf": [{"type": "integer", "minimum": 1}, {"type": "null"}] }
        },
        "required": ["name", "failure_kind", "file", "line", "column"]
    })
}
