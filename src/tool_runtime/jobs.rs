use serde_json::{json, Value};
use webcodex_core::runner_job_lifecycle::RunnerJobLifecycle;

use super::helpers::{
    command_rejected_message, explicit_shell_dispatch_command, is_safe_job_id,
    project_relative_runner_cwd, resolve_runner_cwd, validate_raw_shell_command_length,
};
use super::tool_result::{RecoveryKind, RecoveryTool, ToolResult};
use super::{ExecutionPurpose, ExecutionShell, ToolRuntime};
use crate::auth::AuthContext;
use crate::runner_http::{command_preview, ShellJobStartMetadata, COMMAND_PREVIEW_MAX_CHARS};
use crate::runner_protocol::{
    ShellJobActivity, ShellJobActivityPhase, ShellJobActivitySource, ShellJobActivityState,
    ShellJobInfo, ShellJobOpRequest, ShellJobStructuredExecutionMetadata, ShellJobValidationStep,
};

pub(crate) fn is_blocking_active_job_status(status: &str) -> bool {
    webcodex_runner_registry::job_status_is_active(status) && !is_stop_pending_job_status(status)
}

pub(crate) fn is_stop_pending_job_status(status: &str) -> bool {
    RunnerJobLifecycle::from_wire(status) == Ok(RunnerJobLifecycle::StopRequested)
}

pub(crate) fn is_terminal_job_status(status: &str) -> bool {
    RunnerJobLifecycle::from_wire(status).is_ok_and(RunnerJobLifecycle::is_terminal)
}

pub(crate) fn detected_job_summary(
    command_summary: Option<&str>,
    purpose: Option<&str>,
    status: &str,
    exit_code: Option<i64>,
    stdout: &str,
    stderr: &str,
) -> Value {
    detected_job_summary_with_activity(
        command_summary,
        purpose,
        status,
        exit_code,
        stdout,
        stderr,
        None,
    )
}

fn activity_progress_projection(activity: &ShellJobActivity) -> Value {
    let state = match activity.state {
        ShellJobActivityState::Working => "working",
        ShellJobActivityState::Waiting => "waiting",
    };
    let (reason_code, summary) = match activity.phase {
        ShellJobActivityPhase::ProcessRunning => {
            ("process_running", "Process execution in progress")
        }
        ShellJobActivityPhase::ValidationFormat => (
            "validation_format",
            "Structured format validation in progress",
        ),
        ShellJobActivityPhase::ValidationCheck => (
            "validation_check",
            "Structured check validation in progress",
        ),
        ShellJobActivityPhase::ValidationTest => {
            ("validation_test", "Structured test validation in progress")
        }
        ShellJobActivityPhase::CargoWaitingForBuildLock => (
            "cargo_waiting_for_build_lock",
            "Waiting for Cargo build lock",
        ),
        ShellJobActivityPhase::CargoCompiling => {
            ("cargo_compiling", "Cargo compilation in progress")
        }
        ShellJobActivityPhase::CargoChecking => ("cargo_checking", "Cargo checking in progress"),
    };
    let source = match activity.source {
        ShellJobActivitySource::RunnerExecution => "runner_execution",
        ShellJobActivitySource::ValidationPlan => "validation_plan",
        ShellJobActivitySource::CargoOutput => "cargo_output",
    };
    json!({
        "state": state,
        "reason_code": reason_code,
        "summary": summary,
        "source": source,
    })
}

pub(crate) fn detected_job_summary_with_activity(
    command_summary: Option<&str>,
    purpose: Option<&str>,
    status: &str,
    exit_code: Option<i64>,
    stdout: &str,
    stderr: &str,
    activity: Option<&ShellJobActivity>,
) -> Value {
    let normalized = command_summary
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let kind = if normalized.starts_with("cargo test") {
        "test"
    } else if normalized.starts_with("cargo check") {
        "check"
    } else if normalized.starts_with("cargo fmt") {
        "format"
    } else if normalized.starts_with("cargo build") {
        "build"
    } else {
        match purpose {
            Some("other") | None => "operation",
            Some(purpose) => purpose,
        }
    };
    let lifecycle = RunnerJobLifecycle::from_wire(status).ok();
    let outcome = if !lifecycle.is_some_and(RunnerJobLifecycle::is_terminal) {
        "in_progress"
    } else if lifecycle == Some(RunnerJobLifecycle::Completed) && exit_code == Some(0) {
        "passed"
    } else if lifecycle.is_some_and(RunnerJobLifecycle::is_timed_out) {
        "timed_out"
    } else if matches!(
        lifecycle,
        Some(RunnerJobLifecycle::Stopped | RunnerJobLifecycle::Cancelled)
    ) {
        "cancelled"
    } else {
        "failed"
    };
    let mut detected = json!({
        "kind": kind,
        "outcome": outcome,
    });
    if outcome == "in_progress" {
        let lower = format!("{stdout}\n{stderr}").to_ascii_lowercase();
        let cargo_command = normalized == "cargo" || normalized.starts_with("cargo ");
        let progress = if let Some(activity) = activity {
            Some(activity_progress_projection(activity))
        } else if cargo_command && lower.contains("blocking waiting for file lock") {
            Some(json!({
                "state": "waiting",
                "reason_code": "cargo_build_lock",
                "summary": "Waiting for Cargo build lock",
            }))
        } else if cargo_command
            && matches!(kind, "test" | "check" | "build" | "format")
            && lower
                .lines()
                .any(|line| line.trim_start().starts_with("compiling "))
        {
            Some(json!({
                "state": "working",
                "reason_code": "cargo_compiling",
                "summary": "Cargo compilation in progress",
            }))
        } else if cargo_command
            && matches!(kind, "test" | "check" | "build" | "format")
            && lower
                .lines()
                .any(|line| line.trim_start().starts_with("checking "))
        {
            Some(json!({
                "state": "working",
                "reason_code": "cargo_checking",
                "summary": "Cargo checking in progress",
            }))
        } else {
            None
        };
        if let Some(progress) = progress {
            detected["progress"] = progress;
        }
    }
    if kind == "test" {
        let combined = format!("{stdout}\n{stderr}");
        let metadata = super::cargo::parse_cargo_test_run_metadata(&combined);
        detected["tests_detected"] = json!(metadata.tests_detected);
        detected["tests_run_count"] = json!(metadata.tests_run_count);
        detected["zero_tests_run"] = json!(metadata.zero_tests_run);
        detected["tests_passed"] = json!(metadata.tests_passed);
        detected["tests_failed"] = json!(metadata.tests_failed);
    }
    detected
}

#[cfg(test)]
mod detected_summary_tests {
    use super::{detected_job_summary, detected_job_summary_with_activity};
    use crate::runner_protocol::{
        ShellJobActivity, ShellJobActivityPhase, ShellJobActivitySource, ShellJobActivityState,
    };

    #[test]
    fn cargo_progress_is_advisory_and_command_scoped() {
        let locked = detected_job_summary(
            Some("cargo check -p webcodex"),
            Some("validation"),
            "running",
            None,
            "",
            "Blocking waiting for file lock on build directory\n",
        );
        assert_eq!(locked["progress"]["state"], "waiting");
        assert_eq!(locked["progress"]["reason_code"], "cargo_build_lock");

        let compiling = detected_job_summary(
            Some("cargo test -p webcodex"),
            Some("test"),
            "running",
            None,
            "",
            "   Compiling webcodex v0.3.9\n",
        );
        assert_eq!(compiling["progress"]["reason_code"], "cargo_compiling");

        let unrelated = detected_job_summary(
            Some("custom-tool"),
            Some("operation"),
            "running",
            None,
            "Blocking waiting for file lock on build directory\n",
            "",
        );
        assert!(unrelated.get("progress").is_none());
    }

    #[test]
    fn structured_activity_takes_precedence_over_conflicting_log_heuristics() {
        let activity = ShellJobActivity {
            state: ShellJobActivityState::Waiting,
            phase: ShellJobActivityPhase::CargoWaitingForBuildLock,
            source: ShellJobActivitySource::CargoOutput,
        };
        let detected = detected_job_summary_with_activity(
            Some("cargo check -p webcodex"),
            Some("validation"),
            "running",
            None,
            "",
            "Checking webcodex v0.3.9\n",
            Some(&activity),
        );
        assert_eq!(detected["progress"]["state"], "waiting");
        assert_eq!(
            detected["progress"]["reason_code"],
            "cargo_waiting_for_build_lock"
        );
        assert_eq!(detected["progress"]["source"], "cargo_output");
    }
}

#[derive(Debug, Clone)]
pub(crate) struct StructuredValidationEvidence {
    pub(crate) diagnostics: Option<super::validation_parser::ValidationDiagnostics>,
    pub(crate) tests_detected: Option<bool>,
    pub(crate) tests_run_count: Option<u64>,
    pub(crate) tests_passed: Option<u64>,
    pub(crate) tests_failed: Option<u64>,
    pub(crate) zero_tests_run: Option<bool>,
    pub(crate) test_count_evidence_reason: Option<&'static str>,
    pub(crate) warnings_count: Option<u64>,
    pub(crate) errors_count: Option<u64>,
}

pub(crate) fn structured_validation_evidence(
    tool: &str,
    kind: &str,
    stdout: &str,
    stderr: &str,
    truncated: bool,
) -> StructuredValidationEvidence {
    let combined = format!("{stdout}\n{stderr}");
    let diagnostics = matches!(kind, "check" | "test")
        .then(|| super::validation_profile::validation_adapter_for_tool(tool))
        .flatten()
        .map(|adapter| adapter.parse(stdout, stderr, truncated));
    let mut evidence = StructuredValidationEvidence {
        diagnostics,
        tests_detected: None,
        tests_run_count: None,
        tests_passed: None,
        tests_failed: None,
        zero_tests_run: None,
        test_count_evidence_reason: None,
        warnings_count: None,
        errors_count: None,
    };
    match kind {
        "test" if tool == "go_test" => {
            let test_summary = evidence
                .diagnostics
                .as_ref()
                .and_then(|diagnostics| diagnostics.test_summary.as_ref());
            evidence.tests_detected = Some(test_summary.is_some());
            if !truncated {
                evidence.tests_passed = test_summary.and_then(|summary| summary.passed);
                evidence.tests_failed = test_summary.and_then(|summary| summary.failed);
                evidence.tests_run_count = test_summary.map(|summary| {
                    summary
                        .passed
                        .unwrap_or(0)
                        .saturating_add(summary.failed.unwrap_or(0))
                        .saturating_add(summary.ignored.unwrap_or(0))
                });
                evidence.zero_tests_run = evidence.tests_run_count.map(|count| count == 0);
            }
        }
        "test" => {
            let metadata = super::cargo::parse_cargo_test_run_metadata(&combined);
            evidence.tests_detected = Some(metadata.tests_detected);
            evidence.test_count_evidence_reason = Some(if truncated {
                "output_truncated"
            } else {
                metadata.count_evidence_reason
            });
            if !truncated {
                evidence.tests_run_count = metadata.tests_run_count;
                evidence.tests_passed = metadata.tests_passed;
                evidence.tests_failed = metadata.tests_failed;
                evidence.zero_tests_run = metadata.zero_tests_run;
            }
        }
        "check" if !truncated => {
            evidence.warnings_count =
                Some(super::cargo::count_rustc_diagnostics(&combined, "warning:") as u64);
            evidence.errors_count =
                Some(super::cargo::count_rustc_diagnostics(&combined, "error:") as u64);
        }
        _ => {}
    }
    evidence
}

#[cfg(test)]
pub(crate) fn validation_job_projection(
    tool: Option<&str>,
    kind: Option<&str>,
    status: &str,
    exit_code: Option<i64>,
    stdout: &str,
    stderr: &str,
    truncated: bool,
    minimum_tests: Option<u64>,
) -> Option<Value> {
    validation_job_projection_with_policy(
        tool,
        kind,
        status,
        exit_code,
        stdout,
        stderr,
        truncated,
        minimum_tests,
        None,
        None,
    )
}

pub(crate) fn validation_job_projection_with_policy(
    tool: Option<&str>,
    kind: Option<&str>,
    status: &str,
    exit_code: Option<i64>,
    stdout: &str,
    stderr: &str,
    truncated: bool,
    minimum_tests: Option<u64>,
    require_tests: Option<bool>,
    no_run: Option<bool>,
) -> Option<Value> {
    let tool = tool?;
    let kind = kind.unwrap_or(match tool {
        "cargo_test" | "go_test" => "test",
        "cargo_fmt" => "format",
        _ => "check",
    });
    let lifecycle = RunnerJobLifecycle::from_wire(status).ok();
    if !lifecycle.is_some_and(RunnerJobLifecycle::is_terminal) {
        let mut value = json!({
            "tool": tool,
            "kind": kind,
            "state": if matches!(
                lifecycle,
                Some(RunnerJobLifecycle::Queued | RunnerJobLifecycle::RunnerQueued)
            ) { "pending" } else { "running" },
        });
        apply_cargo_test_execution_policy(&mut value, tool, require_tests, no_run);
        return Some(value);
    }
    if matches!(
        lifecycle,
        Some(
            RunnerJobLifecycle::Timeout
                | RunnerJobLifecycle::TimedOut
                | RunnerJobLifecycle::Stopped
                | RunnerJobLifecycle::Cancelled
                | RunnerJobLifecycle::Lost
        )
    ) {
        let evidence = structured_validation_evidence(tool, kind, stdout, stderr, true);
        let mut value = json!({
            "tool": tool,
            "kind": kind,
            "state": match lifecycle {
                Some(RunnerJobLifecycle::Timeout | RunnerJobLifecycle::TimedOut) => "timed_out",
                Some(RunnerJobLifecycle::Stopped | RunnerJobLifecycle::Cancelled) => "cancelled",
                _ => "lost",
            },
            "passed": Value::Null,
            "truncated": true,
        });
        if let Some(diagnostics) = evidence.diagnostics {
            value["diagnostics"] = json!(diagnostics);
        }
        match kind {
            "test" => {
                value["tests_detected"] = json!(evidence.tests_detected);
                value["tests_run_count"] = Value::Null;
                value["tests_passed"] = Value::Null;
                value["tests_failed"] = Value::Null;
                value["zero_tests_run"] = Value::Null;
            }
            "check" => {
                value["warnings_count"] = Value::Null;
                value["errors_count"] = Value::Null;
            }
            _ => {}
        }
        apply_cargo_test_execution_policy(&mut value, tool, require_tests, no_run);
        return Some(value);
    }
    let process_passed = lifecycle == Some(RunnerJobLifecycle::Completed) && exit_code == Some(0);
    let evidence = structured_validation_evidence(tool, kind, stdout, stderr, truncated);
    let mut passed = process_passed;
    let mut value = json!({
        "tool": tool,
        "kind": kind,
        "state": "completed",
        "passed": passed,
        "truncated": truncated,
    });
    if let Some(diagnostics) = evidence.diagnostics {
        value["diagnostics"] = json!(diagnostics);
    }
    match kind {
        "test" => {
            value["tests_detected"] = json!(evidence.tests_detected);
            value["tests_run_count"] = json!(evidence.tests_run_count);
            value["tests_passed"] = json!(evidence.tests_passed);
            value["tests_failed"] = json!(evidence.tests_failed);
            value["zero_tests_run"] = json!(evidence.zero_tests_run);
            if tool == "cargo_test" && process_passed {
                if let Some(minimum_tests) = minimum_tests {
                    let (status, reason_code) = match evidence.tests_run_count {
                        Some(actual) if actual >= minimum_tests => ("passed", "minimum_satisfied"),
                        Some(_) => {
                            passed = false;
                            ("failed", "minimum_not_met")
                        }
                        None => {
                            passed = false;
                            ("unproven", "test_count_unproven")
                        }
                    };
                    value["passed"] = json!(passed);
                    value["test_count_assertion"] = json!({
                        "minimum_tests": minimum_tests,
                        "actual_tests_run": evidence.tests_run_count,
                        "status": status,
                        "reason_code": reason_code,
                        "evidence_reason_code": evidence
                            .test_count_evidence_reason
                            .unwrap_or("no_complete_summary"),
                    });
                }
            }
        }
        "check" => {
            value["warnings_count"] = json!(evidence.warnings_count);
            value["errors_count"] = json!(evidence.errors_count);
        }
        _ => {}
    }
    apply_cargo_test_execution_policy(&mut value, tool, require_tests, no_run);
    Some(value)
}

fn apply_cargo_test_execution_policy(
    value: &mut Value,
    tool: &str,
    require_tests: Option<bool>,
    no_run: Option<bool>,
) {
    if tool != "cargo_test" {
        return;
    }
    if let Some(require_tests) = require_tests {
        value["require_tests"] = json!(require_tests);
    }
    if let Some(no_run) = no_run {
        value["no_run"] = json!(no_run);
    }
}

fn is_lifecycle_active_status(status: &str) -> bool {
    is_blocking_active_job_status(status) || is_stop_pending_job_status(status)
}

fn add_job_lifecycle_fields(
    output: &mut Value,
    status: &str,
    recovery_state: Option<&str>,
    recovery_reason_code: Option<&str>,
) {
    let blocking_active = is_blocking_active_job_status(status);
    let terminal_pending = is_stop_pending_job_status(status);
    output["active"] = json!(blocking_active || terminal_pending);
    output["blocking_active"] = json!(blocking_active);
    output["terminal"] = json!(is_terminal_job_status(status));
    output["terminal_pending"] = json!(terminal_pending);
    if let Some(text) = recovery_reason_text(recovery_state, recovery_reason_code) {
        output["recovery_reason"] = json!(text);
    }
}

/// Map the bounded `recovery_state` / `recovery_reason_code` pair to a stable,
/// human-readable `recovery_reason` string for the Console/API projection.
///
/// The text is derived only from the bounded reason codes and the recovery
/// state — never from raw backend error strings, tokens, command payloads,
/// environment, filesystem paths, transport connection ids, raw inventory, or
/// internal notifier/request-channel state. Unknown reason codes fall back to
/// a generic form that echoes only the code (safe to surface, not sensitive).
pub(crate) fn recovery_reason_text(
    recovery_state: Option<&str>,
    recovery_reason_code: Option<&str>,
) -> Option<String> {
    match (recovery_state, recovery_reason_code) {
        (Some("recovering"), _) => {
            Some("server is waiting for the same runner instance to reconnect".to_string())
        }
        (Some("reconciled"), _) => Some("reconciled after runner reconnect".to_string()),
        (Some("lost_after_reconcile"), Some(code)) => Some(match code {
            "runner_recovery_deadline_exceeded" => {
                "lost: runner did not reconnect before the recovery deadline".to_string()
            }
            "runner_inventory_missing" => {
                "lost: runner reconnect did not report this job in its inventory".to_string()
            }
            "runner_instance_replaced" => {
                "lost: runner instance was replaced by a newer process".to_string()
            }
            _ => format!("lost after reconciliation ({code})"),
        }),
        (Some("lost_after_reconcile"), None) => Some("lost after reconciliation".to_string()),
        // Jobs lost without entering the reconciliation path keep their original
        // reason code and have recovery_state == None.
        (_, Some("runner_disconnected_without_reconciliation")) => {
            Some("lost: runner disconnected without reconciliation support".to_string())
        }
        (_, Some("runner_transport_disconnected")) => {
            Some("lost: runner transport disconnected".to_string())
        }
        (_, Some("runner_transport_stale")) => {
            Some("lost: runner transport went stale while the job was running".to_string())
        }
        (_, Some("runner_request_not_dispatched")) => {
            Some("lost: runner did not dispatch the job request".to_string())
        }
        (_, Some(code)) => Some(format!("recovery ({code})")),
        (Some(state), None) => Some(format!("recovery ({state})")),
        (None, None) => None,
    }
}

fn command_preview_truncated(preview: &str) -> bool {
    preview.chars().count() > COMMAND_PREVIEW_MAX_CHARS
}

fn add_command_preview_metadata(output: &mut Value, preview: String) {
    output["command_preview_truncated"] = json!(command_preview_truncated(&preview));
    output["command_preview_max_chars"] = json!(COMMAND_PREVIEW_MAX_CHARS);
    output["command_preview_bounded"] = json!(true);
    output["command_preview"] = Value::String(preview);
}

fn agent_log_stream_incomplete(
    runner_truncated: bool,
    retained_from_line: Option<usize>,
    returned_lines: usize,
    next_line: usize,
) -> bool {
    runner_truncated
        || retained_from_line.is_some_and(|line| line > 1)
        || returned_lines < next_line.saturating_sub(1)
}

fn model_facing_structured_execution_metadata(
    metadata: Option<&ShellJobStructuredExecutionMetadata>,
) -> Value {
    let Some(metadata) = metadata else {
        return Value::Null;
    };
    json!({
        "execution_source": metadata.execution_source,
        "language": metadata.language,
        "script_bytes": metadata.script_bytes,
        "arg_count": metadata.arg_count,
        "stdin_present": metadata.stdin_present,
    })
}

/// Build a bounded job summary `Value` for an agent-known job. Never includes
/// stdout/stderr bodies.
pub(crate) fn agent_job_summary_value(job: &ShellJobInfo) -> Value {
    json!({
        "job_id": job.job_id,
        "kind": job.kind,
        "status": job.status,
        "project": job.project_id,
        "session_id": job.session_id,
        "ssh_resource": job.ssh_resource,
        "executor": "agent",
        "client_id": job.client_id,
        "created_at": job.created_at,
        "started_at": job.started_at,
        "ended_at": job.ended_at,
        "duration_ms": job.duration_ms,
        "elapsed_secs": job.elapsed_secs,
        "exit_code": job.exit_code,
        "command_execution_state": job.command_execution_state,
        "structured_execution": model_facing_structured_execution_metadata(job.structured_execution.as_ref()),
        "activity": job.activity,
        "recovery_state": job.recovery_state,
        "recovered_after_server_restart": job.recovered_after_server_restart,
        "reconciled_at": job.reconciled_at,
        "recovery_reason_code": job.recovery_reason_code,
        "last_update_seq": job.last_update_seq,
        "recovery_reason": recovery_reason_text(
            job.recovery_state.as_deref(),
            job.recovery_reason_code.as_deref(),
        ),
    })
}

fn invalid_job_observation_result(error_kind: &str, message: String) -> ToolResult {
    ToolResult::err_with_output(
        message,
        json!({
            "error_kind": error_kind,
            "failure_kind": "invalid_arguments",
            "state_changed": false,
        }),
    )
    .with_recovery(RecoveryKind::FixInput, None)
}

fn unknown_job_observation_result(job_id: &str) -> ToolResult {
    ToolResult::err_with_output(
        format!("unknown job: {}", job_id),
        json!({
            "error_kind": "unknown_job",
            "failure_kind": "job_not_found",
            "job_id": job_id,
            "state_changed": false,
        }),
    )
    .with_recovery(RecoveryKind::Reobserve, Some(RecoveryTool::ListJobs))
}

fn agent_job_log_error_result(job_id: &str, error: String) -> ToolResult {
    if error.starts_with("invalid after_observation_token:") {
        invalid_job_observation_result("invalid_observation_token", error)
    } else {
        unknown_job_observation_result(job_id)
    }
}

fn confirmation_required_result(project: &str, job_id: &str) -> ToolResult {
    ToolResult::err_with_output(
        "confirmation_required: stop_job requires confirm=true".to_string(),
        json!({
            "error_kind": "confirmation_required",
            "failure_kind": "confirmation_required",
            "project": project,
            "job_id": job_id,
            "already_finished": false,
            "already_stop_requested": false,
            "stop_request_accepted": false,
            "target_was_active_at_request": false,
            "terminal": false,
            "terminal_pending": false,
            "final_status": Value::Null,
            "stop_effect": "confirmation_required",
            "command_started": false,
        }),
    )
    .with_recovery(RecoveryKind::UserAction, None)
}

fn job_not_found_result(project: &str, job_id: &str) -> ToolResult {
    ToolResult::err_with_output(
        format!("job_not_found: {}", job_id),
        json!({
            "error_kind": "job_not_found",
            "failure_kind": "job_not_found",
            "project": project,
            "job_id": job_id,
            "already_finished": false,
            "already_stop_requested": false,
            "stop_request_accepted": false,
            "target_was_active_at_request": false,
            "terminal": false,
            "terminal_pending": false,
            "final_status": Value::Null,
            "stop_effect": "not_found",
            "command_started": false,
        }),
    )
    .with_recovery(RecoveryKind::Reobserve, Some(RecoveryTool::ListJobs))
}

fn job_project_mismatch_result(
    request_project: &str,
    job_project: &str,
    job_id: &str,
) -> ToolResult {
    ToolResult::err_with_output(
        format!(
            "job_project_mismatch: job {} belongs to project {} but request used {}",
            job_id, job_project, request_project
        ),
        json!({
            "error_kind": "job_project_mismatch",
            "failure_kind": "job_project_mismatch",
            "project": request_project,
            "job_project": job_project,
            "job_id": job_id,
            "already_finished": false,
            "already_stop_requested": false,
            "stop_request_accepted": false,
            "target_was_active_at_request": false,
            "terminal": false,
            "terminal_pending": false,
            "final_status": Value::Null,
            "stop_effect": "forbidden",
            "command_started": false,
        }),
    )
    .with_recovery(RecoveryKind::FixInput, None)
}

fn job_stop_forbidden_result(
    request_project: &str,
    job_id: &str,
    request_session_id: Option<&str>,
    job_session_id: Option<&str>,
) -> ToolResult {
    ToolResult::err_with_output(
        format!(
            "job_stop_forbidden: job {} is bound to a different session",
            job_id
        ),
        json!({
            "error_kind": "job_stop_forbidden",
            "failure_kind": "job_stop_forbidden",
            "project": request_project,
            "job_id": job_id,
            "request_session_id": request_session_id,
            "job_session_id": job_session_id,
            "already_finished": false,
            "already_stop_requested": false,
            "stop_request_accepted": false,
            "target_was_active_at_request": false,
            "terminal": false,
            "terminal_pending": false,
            "final_status": Value::Null,
            "stop_effect": "forbidden",
            "command_started": false,
        }),
    )
    .with_recovery(RecoveryKind::FixInput, None)
}

fn job_session_unknown_warning() -> Value {
    json!({
        "kind": "job_session_unknown",
        "warning_kind": "job_session_unknown",
        "message": "job has no recorded session_id; stop authorized by project boundary only",
    })
}

fn job_recovering_stop_result(project: &str, job: &ShellJobInfo) -> ToolResult {
    ToolResult::err_with_output(
        "runner_unavailable_recovering: the runner must reconcile this job before it can be stopped"
            .to_string(),
        json!({
            "error_kind": "runner_unavailable_recovering",
            "failure_kind": "runner_unavailable_recovering",
            "project": project,
            "job_id": job.job_id,
            "status_before": "recovering",
            "status_after": "recovering",
            "recovery_state": job.recovery_state,
            "recovery_reason_code": job.recovery_reason_code,
            "recovery_reason": recovery_reason_text(
                job.recovery_state.as_deref(),
                job.recovery_reason_code.as_deref(),
            ),
            "already_finished": false,
            "already_stop_requested": false,
            "stop_request_accepted": false,
            "target_was_active_at_request": true,
            "terminal": false,
            "terminal_pending": false,
            "final_status": Value::Null,
            "stop_effect": "runner_unavailable",
            "command_started": false,
        }),
    )
    .with_recovery(RecoveryKind::Wait, None)
}

fn ownership_basis_for_stop(
    request_project: &str,
    job_id: &str,
    request_session_id: Option<&str>,
    job_session_id: Option<&str>,
) -> Result<(&'static str, Vec<Value>), ToolResult> {
    match job_session_id {
        Some(job_session_id) if Some(job_session_id) == request_session_id => {
            Ok(("project_and_session", Vec::new()))
        }
        Some(job_session_id) => Err(job_stop_forbidden_result(
            request_project,
            job_id,
            request_session_id,
            Some(job_session_id),
        )),
        None => Ok((
            "unknown_session_project_only",
            vec![job_session_unknown_warning()],
        )),
    }
}

fn stop_job_output(
    project: &str,
    job_id: &str,
    status_before: &str,
    status_after: &str,
    stopped: bool,
    already_finished: bool,
    ownership_basis: &str,
    warnings: Vec<Value>,
) -> Value {
    let already_stop_requested = is_stop_pending_job_status(status_before) && !already_finished;
    let terminal = is_terminal_job_status(status_after);
    let terminal_pending = is_stop_pending_job_status(status_after);
    let stop_request_accepted = !already_finished && !already_stop_requested && stopped;
    let stop_effect = if already_finished {
        "already_finished"
    } else if already_stop_requested {
        "already_stop_requested"
    } else if terminal {
        "stopped"
    } else {
        "requested"
    };
    let mut output = json!({
        "already_finished": already_finished,
        "already_stop_requested": already_stop_requested,
        "stop_request_accepted": stop_request_accepted,
        "target_was_active_at_request": is_lifecycle_active_status(status_before),
        "terminal": terminal,
        "terminal_pending": terminal_pending,
        "final_status": if terminal { json!(status_after) } else { Value::Null },
        "stop_effect": stop_effect,
        "job_id": job_id,
        "project": project,
        "status_before": status_before,
        "status_after": status_after,
        "command_started": false,
        "ownership_basis": ownership_basis,
    });
    if !warnings.is_empty() {
        output["warning_kind"] = warnings
            .first()
            .and_then(|warning| warning.get("warning_kind"))
            .cloned()
            .unwrap_or(Value::Null);
        output["warnings"] = Value::Array(warnings);
    }
    output
}

fn active_job_brief(summary: &Value) -> Value {
    json!({
        "job_id": summary.get("job_id").cloned().unwrap_or(Value::Null),
        "kind": summary.get("kind").cloned().unwrap_or_else(|| json!("shell")),
        "status": summary.get("status").cloned().unwrap_or(Value::Null),
        "project": summary.get("project").cloned().unwrap_or(Value::Null),
        "started_at": summary.get("started_at").cloned().unwrap_or(Value::Null),
        "created_at": summary.get("created_at").cloned().unwrap_or(Value::Null),
        "executor": summary.get("executor").cloned().unwrap_or(Value::Null),
    })
}

impl ToolRuntime {
    pub(crate) async fn run_job_for_auth(
        &self,
        project: String,
        command: String,
        session_id: Option<String>,
        timeout_secs: Option<i64>,
        cwd: Option<String>,
        validation_steps: Vec<ShellJobValidationStep>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        self.run_job_for_auth_with_contract(
            project,
            command,
            session_id,
            timeout_secs,
            cwd,
            validation_steps,
            auth,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_job_for_auth_with_contract(
        &self,
        project: String,
        command: String,
        session_id: Option<String>,
        timeout_secs: Option<i64>,
        cwd: Option<String>,
        validation_steps: Vec<ShellJobValidationStep>,
        auth: Option<&AuthContext>,
        purpose: Option<ExecutionPurpose>,
        shell: Option<ExecutionShell>,
    ) -> ToolResult {
        self.run_job_for_auth_with_contract_with_ssh_resource(
            project,
            command,
            session_id,
            timeout_secs,
            cwd,
            validation_steps,
            auth,
            purpose,
            shell,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_job_for_auth_with_contract_with_ssh_resource(
        &self,
        project: String,
        command: String,
        session_id: Option<String>,
        timeout_secs: Option<i64>,
        cwd: Option<String>,
        validation_steps: Vec<ShellJobValidationStep>,
        auth: Option<&AuthContext>,
        purpose: Option<ExecutionPurpose>,
        shell: Option<ExecutionShell>,
        ssh_resource: Option<&str>,
    ) -> ToolResult {
        if let Err(error) = validate_raw_shell_command_length(&command) {
            return ToolResult::err(command_rejected_message(
                error,
                "use run_script for larger shell program text or stdin/files/artifacts for large data.",
            ));
        }
        let resolved = match self.resolve_project_input_for_auth(&project, auth).await {
            Ok(resolved) => resolved,
            Err(e) => return ToolResult::err(command_rejected_message(
                e.to_message(),
                "verify the project id with list_projects, then retry with a registered project.",
            )),
        };
        let project_id = resolved.resolved_id.clone();
        let proj = resolved.config;
        let max_runtime = timeout_secs.unwrap_or(3600).clamp(1, 604800);
        let declared_purpose = purpose.unwrap_or_default();
        let command_summary = command_preview(&command);
        let client_id = proj.client_id.clone();
        let remote = ssh_resource.is_some();
        if remote && !validation_steps.is_empty() {
            return ToolResult::err(
                    "ssh_resource_unsupported_for_request: SSH resources do not support structured validation jobs"
                        .to_string(),
                );
        }
        if remote && session_id.is_none() {
            return ToolResult::err(
                    "ssh_session_required: an SSH resource requires a Workflow Session id; command was not started"
                        .to_string(),
                );
        }
        let (effective_cwd, resolved_cwd) = if remote {
            let remote_cwd = cwd
                .as_deref()
                .map(str::trim)
                .filter(|cwd| !cwd.is_empty())
                .map(str::to_string);
            if remote_cwd
                .as_deref()
                .is_some_and(|cwd| cwd.len() > 4096 || cwd.chars().any(char::is_control))
            {
                return ToolResult::err(command_rejected_message(
                        "ssh_remote_cwd_invalid: cwd must be a bounded remote path without control characters",
                        "choose a valid remote path, or omit cwd to use the SSH resource default.",
                    ));
            }
            let display = remote_cwd.clone().unwrap_or_else(|| ".".to_string());
            (remote_cwd, display)
        } else {
            let cwd = match resolve_runner_cwd(&proj, cwd.as_deref()) {
                    Ok(cwd) => cwd,
                    Err(error) => {
                        return ToolResult::err(command_rejected_message(
                            error,
                            "choose '.', an existing project-relative cwd, or a path inside the registered project root.",
                        ))
                    }
                };
            let display =
                project_relative_runner_cwd(&proj, &cwd).unwrap_or_else(|_| ".".to_string());
            (Some(cwd), display)
        };
        let actual_shell = shell.map(ExecutionShell::as_str).unwrap_or(if remote {
            "remote"
        } else {
            "configured"
        });
        let dispatched_command = match shell {
            Some(shell) => match explicit_shell_dispatch_command(&command, shell.as_str()) {
                Ok(command) => command,
                Err(error) => {
                    return ToolResult::err(command_rejected_message(
                        error,
                        "use run_script for large or quote-dense explicit-shell program text.",
                    ))
                }
            },
            None => command.clone(),
        };
        match self
                .runner_registry
                .start_job_with_metadata_for_access(
                    ShellJobOpRequest {
                        op: "start".to_string(),
                        client_id: Some(client_id),
                        cwd: effective_cwd,
                        command: Some(dispatched_command),
                        timeout_secs: Some(max_runtime as u64),
                        job_id: None,
                        since_stdout_line: None,
                        since_stderr_line: None,
                        tail_lines: None,
                        limit: None,
                        codex: None,
                    },
                    "tool_runtime".to_string(),
                    ShellJobStartMetadata {
                        project_id: Some(project_id.clone()),
                        session_id: session_id.clone(),
                        ssh_resource: ssh_resource.map(str::to_string),
                        project_cwd: Some(resolved_cwd.clone()),
                        purpose: Some(declared_purpose.as_str().to_string()),
                        shell: Some(actual_shell.to_string()),
                        validation_steps,
                        validation: None,
                        visibility: crate::runner_http::ShellJobVisibility::Public,
                        validation_identity: None,
                        validation_tool: None,
                        assertion_name: None,
                        structured_execution: None,
                        stdin: None,
                        detached_idempotency_key: None,
                    },
                    crate::runner_http::runner_access_from_auth(auth).as_ref(),
                    None,
                )
                .await
            {
                Ok(job) => ToolResult::ok(json!({
                    "job_id": job.job_id,
                    "kind": job.kind,
                    "status": job.status,
                    "project": project_id,
                    "execution_source": "run_job",
                    "purpose": declared_purpose.as_str(),
                    "command_summary": command_summary,
                    "cwd": resolved_cwd,
                    "shell": actual_shell,
                    "executor": "agent",
                    "ssh_resource": ssh_resource,
                    "execution_state": "started",
                    "created_at": job.created_at,
                    "observation_token": job.observation_token,
                    "last_update_seq": job.last_update_seq,
                    "stdout_tail": "",
                    "stderr_tail": "",
                    "stdout_lines": 0,
                    "stderr_lines": 0,
                    "stdout_truncated": false,
                    "stderr_truncated": false,
                })),
                Err(e) => ToolResult::err(command_rejected_message(
                    e,
                    "confirm the agent is connected and async jobs are allowed, then retry or use run_shell for short commands.",
                )),
            }
    }

    #[cfg(test)]
    pub(crate) async fn job_status(&self, job_id: String) -> ToolResult {
        self.job_status_for_auth(job_id, false, None).await
    }

    pub(crate) async fn job_status_for_auth(
        &self,
        job_id: String,
        include_command_preview: bool,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        match self
            .runner_registry
            .get_job_for_auth(
                crate::runner_http::runner_access_from_auth(auth).as_ref(),
                &job_id,
            )
            .await
        {
            Ok(job) => {
                let mut output = json!({
                    "job_id": job.job_id,
                    "project": job.project_id,
                    "session_id": job.session_id,
                    "ssh_resource": job.ssh_resource,
                    "status": job.status,
                    "exit_code": job.exit_code,
                    "started_at": job.started_at,
                    "ended_at": job.ended_at,
                    "duration_ms": job.duration_ms,
                    "elapsed_secs": job.elapsed_secs,
                    "client_id": job.client_id,
                    "error": job.error,
                    "command_execution_state": job.command_execution_state,
                    "structured_execution": model_facing_structured_execution_metadata(job.structured_execution.as_ref()),
                    "activity": job.activity,
                    "recovery_state": job.recovery_state,
                    "recovered_after_server_restart": job.recovered_after_server_restart,
                    "reconciled_at": job.reconciled_at,
                    "recovery_reason_code": job.recovery_reason_code,
                    "observation_token": job.observation_token,
                    "last_update_seq": job.last_update_seq,
                    "stdout_retained_from_line": job.stdout_retained_from_line,
                    "stderr_retained_from_line": job.stderr_retained_from_line,
                    "stdout_log_truncated": job.stdout_log_truncated,
                    "stderr_log_truncated": job.stderr_log_truncated,
                    "command_preview_included": include_command_preview,
                });
                let status = output["status"].as_str().unwrap_or_default().to_string();
                add_job_lifecycle_fields(
                    &mut output,
                    &status,
                    job.recovery_state.as_deref(),
                    job.recovery_reason_code.as_deref(),
                );
                if include_command_preview {
                    add_command_preview_metadata(&mut output, job.command_preview.clone());
                }
                let validation_metadata = job.validation.as_ref();
                let tool = validation_metadata.map(|metadata| metadata.tool.as_str());
                let kind = validation_metadata.map(|metadata| metadata.kind.as_str());
                if tool.is_some() {
                    let logs = self
                        .runner_registry
                        .job_log_for_auth(
                            crate::runner_http::runner_access_from_auth(auth).as_ref(),
                            &job_id,
                            None,
                            None,
                            Some(500),
                            None,
                            None,
                        )
                        .await;
                    let (stdout, stderr, truncated) = match logs {
                        Ok((logged_job, stdout, stderr, next_stdout_line, next_stderr_line, _)) => {
                            let stdout = stdout.unwrap_or_default();
                            let stderr = stderr.unwrap_or_default();
                            let truncated = agent_log_stream_incomplete(
                                logged_job.stdout_log_truncated,
                                logged_job.stdout_retained_from_line,
                                stdout.lines().count(),
                                next_stdout_line,
                            ) || agent_log_stream_incomplete(
                                logged_job.stderr_log_truncated,
                                logged_job.stderr_retained_from_line,
                                stderr.lines().count(),
                                next_stderr_line,
                            );
                            (stdout, stderr, truncated)
                        }
                        Err(_) => (String::new(), String::new(), true),
                    };
                    if let Some(mut validation) = validation_job_projection_with_policy(
                        tool,
                        kind,
                        &status,
                        job.exit_code.map(i64::from),
                        &stdout,
                        &stderr,
                        truncated,
                        validation_metadata.and_then(|metadata| metadata.minimum_tests),
                        validation_metadata.and_then(|metadata| metadata.require_tests),
                        validation_metadata.and_then(|metadata| metadata.no_run),
                    ) {
                        if let Some(target_id) = validation_metadata
                            .and_then(|metadata| metadata.validation_target_id.as_deref())
                        {
                            validation["validation_target_id"] = json!(target_id);
                        }
                        output["validation"] = validation;
                    }
                }
                ToolResult::ok(output)
            }
            Err(_) => unknown_job_observation_result(&job_id),
        }
    }

    #[cfg(test)]
    pub(crate) async fn job_log(
        &self,
        job_id: String,
        offset: Option<usize>,
        tail_lines: Option<usize>,
    ) -> ToolResult {
        self.job_log_for_auth(job_id, offset, tail_lines, None, None, None)
            .await
    }

    /// Validate the bounded-wait arguments before any execution. Rejects
    /// out-of-range `wait_secs` up front. Observation-token syntax and Job
    /// binding are validated before execution or waiting by the selected executor.
    fn validate_job_log_wait(wait_secs: Option<u64>) -> Result<(), String> {
        if let Some(secs) = wait_secs {
            if secs == 0 || secs > 60 {
                return Err(format!(
                    "invalid wait_secs: must be between 1 and 60, got {}",
                    secs
                ));
            }
        }
        Ok(())
    }

    pub(crate) async fn job_log_for_auth(
        &self,
        job_id: String,
        offset: Option<usize>,
        tail_lines: Option<usize>,
        auth: Option<&AuthContext>,
        after_observation_token: Option<String>,
        wait_secs: Option<u64>,
    ) -> ToolResult {
        if let Err(message) = Self::validate_job_log_wait(wait_secs) {
            return invalid_job_observation_result("invalid_wait_secs", message);
        }
        let tail_lines = if offset.is_none() && tail_lines.is_none() {
            Some(super::helpers::DEFAULT_JOB_LOG_TAIL_LINES)
        } else {
            tail_lines
        };
        match self
            .runner_registry
            .job_log_for_auth(
                crate::runner_http::runner_access_from_auth(auth).as_ref(),
                &job_id,
                offset,
                None,
                tail_lines,
                after_observation_token.as_deref(),
                wait_secs,
            )
            .await
        {
            Ok((job, stdout, stderr, next_stdout_line, next_stderr_line, wait)) => {
                let stdout = stdout.unwrap_or_default();
                let stderr = stderr.unwrap_or_default();
                let command_summary = job.command_preview.clone();
                let purpose = job.purpose.clone().unwrap_or_else(|| "other".to_string());
                let detected_summary = detected_job_summary_with_activity(
                    Some(&command_summary),
                    Some(&purpose),
                    &job.status,
                    job.exit_code.map(i64::from),
                    &wait.analysis_stdout,
                    &wait.analysis_stderr,
                    job.activity.as_ref(),
                );
                let validation_tool = job
                    .validation
                    .as_ref()
                    .map(|metadata| metadata.tool.as_str());
                let validation_kind = job
                    .validation
                    .as_ref()
                    .map(|metadata| metadata.kind.as_str());
                let mut validation = validation_job_projection_with_policy(
                    validation_tool,
                    validation_kind,
                    &job.status,
                    job.exit_code.map(i64::from),
                    &wait.analysis_stdout,
                    &wait.analysis_stderr,
                    wait.analysis_truncated,
                    job.validation
                        .as_ref()
                        .and_then(|metadata| metadata.minimum_tests),
                    job.validation
                        .as_ref()
                        .and_then(|metadata| metadata.require_tests),
                    job.validation.as_ref().and_then(|metadata| metadata.no_run),
                );
                if let (Some(validation), Some(target_id)) = (
                    validation.as_mut(),
                    job.validation
                        .as_ref()
                        .and_then(|metadata| metadata.validation_target_id.as_deref()),
                ) {
                    validation["validation_target_id"] = json!(target_id);
                }
                ToolResult::ok(json!({
                    "job_id": job.job_id,
                    "status": job.status,
                    "exit_code": job.exit_code,
                    "command_execution_state": job.command_execution_state,
                    "structured_execution": model_facing_structured_execution_metadata(job.structured_execution.as_ref()),
                    "activity": job.activity,
                    "stdout_tail": stdout,
                    "stderr_tail": stderr,
                    "stdout_lines": next_stdout_line.saturating_sub(1),
                    "stderr_lines": next_stderr_line.saturating_sub(1),
                    "stdout_returned_lines": wait.stdout_returned_lines,
                    "stderr_returned_lines": wait.stderr_returned_lines,
                    "stdout_truncated": wait.stdout_truncated,
                    "stderr_truncated": wait.stderr_truncated,
                    "stdout_retained_from_line": job.stdout_retained_from_line,
                    "stderr_retained_from_line": job.stderr_retained_from_line,
                    "earlier_stdout_unavailable": job
                        .stdout_retained_from_line
                        .is_some_and(|line| line > 1)
                        || job.stdout_log_truncated,
                    "earlier_stderr_unavailable": job
                        .stderr_retained_from_line
                        .is_some_and(|line| line > 1)
                        || job.stderr_log_truncated,
                    "recovery_state": job.recovery_state,
                    "recovery_reason_code": job.recovery_reason_code,
                    "recovery_reason": recovery_reason_text(
                        job.recovery_state.as_deref(),
                        job.recovery_reason_code.as_deref(),
                    ),
                    "observation_token": job.observation_token,
                    "log_delta_status": wait.log_delta_status.as_str(),
                    "stdout_delta_reset": wait.stdout_delta_reset,
                    "stderr_delta_reset": wait.stderr_delta_reset,
                    "last_update_seq": job.last_update_seq,
                    "cursor": {
                        "stdout": next_stdout_line,
                        "stderr": next_stderr_line,
                    },
                    "wait_outcome": wait.wait_outcome.as_str(),
                    "waited_ms": wait.waited_ms,
                    "changed": wait.changed,
                    "terminal": wait.terminal,
                    "executor": "agent",
                    "session_id": job.session_id,
                    "ssh_resource": job.ssh_resource,
                    "cwd": job.project_cwd,
                    "shell": job.shell,
                    "purpose": purpose,
                    "command_summary": command_summary,
                    "detected_summary": detected_summary,
                    "validation": validation,
                }))
            }
            Err(error) => agent_job_log_error_result(&job_id, error),
        }
    }

    #[cfg(test)]
    pub(crate) async fn list_jobs_for_auth(
        &self,
        limit: Option<usize>,
        status: Option<String>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        self.list_jobs_for_auth_with_filters(limit, status, None, None, auth)
            .await
    }

    pub(crate) async fn list_jobs_for_auth_with_filters(
        &self,
        limit: Option<usize>,
        status: Option<String>,
        project: Option<String>,
        session_id: Option<String>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let max = limit.unwrap_or(20).clamp(1, 100);
        let status_filter = status
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let project_filter = match project {
            Some(value) => {
                let value = value.trim();
                if value.is_empty() || value.chars().count() > 512 {
                    return invalid_job_observation_result(
                        "invalid_project_filter",
                        "invalid_project_filter: project must contain 1..=512 characters"
                            .to_string(),
                    );
                }
                Some(value.to_string())
            }
            None => None,
        };
        let session_filter = match session_id {
            Some(value) => {
                let value = value.trim();
                if value.is_empty() || value.chars().count() > 128 {
                    return invalid_job_observation_result(
                        "invalid_session_filter",
                        "invalid_session_filter: session_id must contain 1..=128 characters"
                            .to_string(),
                    );
                }
                Some(value.to_string())
            }
            None => None,
        };

        // Authorization/visibility is applied by the registry first. Focused
        // filters only reduce that already-visible set, and limit is applied
        // after every filter so exact project/session targets cannot be hidden
        // behind unrelated recent Jobs.
        let agent_jobs = self
            .runner_registry
            .list_all_jobs_for_auth(crate::runner_http::runner_access_from_auth(auth).as_ref())
            .await;
        let mut summaries: Vec<Value> = agent_jobs
            .iter()
            .filter(|job| {
                status_filter
                    .as_ref()
                    .map(|status| status == &job.status)
                    .unwrap_or(true)
                    && project_filter
                        .as_deref()
                        .map(|project| job.project_id.as_deref() == Some(project))
                        .unwrap_or(true)
                    && session_filter
                        .as_deref()
                        .map(|session_id| job.session_id.as_deref() == Some(session_id))
                        .unwrap_or(true)
            })
            .map(agent_job_summary_value)
            .collect();

        summaries.sort_by(|a, b| {
            b["created_at"]
                .as_i64()
                .unwrap_or(0)
                .cmp(&a["created_at"].as_i64().unwrap_or(0))
                .then_with(|| {
                    a["job_id"]
                        .as_str()
                        .unwrap_or_default()
                        .cmp(b["job_id"].as_str().unwrap_or_default())
                })
        });
        let matched_count = summaries.len();
        let truncated = matched_count > max;
        summaries.truncate(max);
        ToolResult::ok(json!({
            "jobs": summaries,
            "count": summaries.len(),
            "matched_count": matched_count,
            "truncated": truncated,
        }))
    }

    pub(crate) async fn validation_job_candidates_for_sessions(
        &self,
        project: &str,
        session_ids: &[String],
        auth: Option<&AuthContext>,
    ) -> std::collections::HashMap<String, Vec<Value>> {
        let requested = session_ids
            .iter()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>();
        let mut grouped = std::collections::HashMap::<String, Vec<Value>>::new();
        if requested.is_empty() {
            return grouped;
        }
        for job in self
            .runner_registry
            .list_all_jobs_for_auth(crate::runner_http::runner_access_from_auth(auth).as_ref())
            .await
            .iter()
        {
            let Some(session_id) = job.session_id.as_deref() else {
                continue;
            };
            let generic_validation = job
                .structured_execution
                .as_ref()
                .and_then(|metadata| metadata.validation_identity.as_deref())
                .is_some()
                && job.purpose.as_deref().is_some_and(|purpose| {
                    matches!(
                        purpose,
                        "validation" | "test" | "build" | "format" | "release"
                    )
                });
            if job.project_id.as_deref() == Some(project)
                && requested.contains(session_id)
                && (job.validation.is_some() || generic_validation)
            {
                let mut summary = agent_job_summary_value(job);
                // Internal validation reconciliation needs the admission-derived correlation
                // metadata; ordinary Job model-facing projections intentionally expose only
                // the execution-shape subset above.
                summary["structured_execution"] = json!(job.structured_execution);
                summary["purpose"] = json!(job.purpose);
                summary["cwd"] = json!(job.project_cwd);
                summary["shell"] = json!(job.shell);
                summary["command_summary"] = json!(job.command_preview);
                grouped
                    .entry(session_id.to_string())
                    .or_default()
                    .push(summary);
            }
        }
        for summaries in grouped.values_mut() {
            summaries.sort_by(|a, b| {
                b["created_at"]
                    .as_i64()
                    .unwrap_or(0)
                    .cmp(&a["created_at"].as_i64().unwrap_or(0))
                    .then_with(|| {
                        a["job_id"]
                            .as_str()
                            .unwrap_or_default()
                            .cmp(b["job_id"].as_str().unwrap_or_default())
                    })
            });
        }
        grouped
    }

    pub(crate) async fn job_tail_for_auth(
        &self,
        job_id: String,
        tail_lines: Option<usize>,
        auth: Option<&AuthContext>,
        after_observation_token: Option<String>,
        wait_secs: Option<u64>,
    ) -> ToolResult {
        let tail = tail_lines.unwrap_or(200).clamp(1, 500);
        self.job_log_for_auth(
            job_id,
            None,
            Some(tail),
            auth,
            after_observation_token,
            wait_secs,
        )
        .await
    }

    /// Model-facing `stop_job`: requires confirm=true, verifies project/session
    /// ownership, and never exposes stdout/stderr.
    pub(crate) async fn stop_job_model_facing(
        &self,
        project: String,
        job_id: String,
        session_id: Option<String>,
        confirm: bool,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        if !confirm {
            return confirmation_required_result(&project, &job_id);
        }
        if !is_safe_job_id(&job_id) {
            return job_not_found_result(&project, &job_id);
        }

        let resolved = match self.resolve_project_input_for_auth(&project, auth).await {
            Ok(resolved) => resolved,
            Err(err) => return err.into_tool_result(),
        };
        let request_project = resolved.resolved_id;
        let job = match self
            .runner_registry
            .get_job_for_auth(
                crate::runner_http::runner_access_from_auth(auth).as_ref(),
                &job_id,
            )
            .await
        {
            Ok(job) => job,
            Err(_) => return job_not_found_result(&request_project, &job_id),
        };
        let Some(job_project) = job
            .project_id
            .as_deref()
            .map(str::trim)
            .filter(|project| !project.is_empty())
        else {
            return job_stop_forbidden_result(
                &request_project,
                &job_id,
                session_id.as_deref(),
                job.session_id.as_deref(),
            );
        };
        if job_project != request_project {
            return job_project_mismatch_result(&request_project, job_project, &job_id);
        }
        let (ownership_basis, warnings) = match ownership_basis_for_stop(
            &request_project,
            &job_id,
            session_id.as_deref(),
            job.session_id.as_deref(),
        ) {
            Ok(value) => value,
            Err(result) => return result,
        };
        let status_before = job.status.clone();
        if status_before == "recovering" {
            return job_recovering_stop_result(&request_project, &job);
        }
        if is_stop_pending_job_status(&status_before) {
            return ToolResult::ok(stop_job_output(
                &request_project,
                &job_id,
                &status_before,
                &status_before,
                true,
                false,
                ownership_basis,
                warnings,
            ));
        }
        if !webcodex_runner_registry::job_status_is_active(&status_before) {
            return ToolResult::ok(stop_job_output(
                &request_project,
                &job_id,
                &status_before,
                &status_before,
                false,
                true,
                ownership_basis,
                warnings,
            ));
        }
        let stopped = match self
            .runner_registry
            .stop_job_for_auth(
                crate::runner_http::runner_access_from_auth(auth).as_ref(),
                &job_id,
                "tool_runtime".to_string(),
            )
            .await
        {
            Ok(job) => job,
            Err(error) if error.contains("runner_unavailable_recovering") => {
                let recovering = self
                    .runner_registry
                    .get_job_for_auth(
                        crate::runner_http::runner_access_from_auth(auth).as_ref(),
                        &job_id,
                    )
                    .await
                    .unwrap_or(job);
                return job_recovering_stop_result(&request_project, &recovering);
            }
            Err(_) => return job_not_found_result(&request_project, &job_id),
        };
        ToolResult::ok(stop_job_output(
            &request_project,
            &job_id,
            &status_before,
            &stopped.status,
            true,
            false,
            ownership_basis,
            warnings,
        ))
    }

    /// Bounded active job summary for finish/handoff. Never returns stdout,
    /// stderr, tails, command text, or command previews.
    pub(crate) async fn active_jobs_summary(
        &self,
        project: Option<&str>,
        auth: Option<&AuthContext>,
        limit: usize,
    ) -> Value {
        let max = limit.clamp(1, 20);
        let mut active = Vec::new();
        for job in self
            .runner_registry
            .list_jobs_for_auth(
                crate::runner_http::runner_access_from_auth(auth).as_ref(),
                Some(100),
            )
            .await
        {
            if !webcodex_runner_registry::job_status_is_active(&job.status) {
                continue;
            }
            if let Some(project) = project {
                if job.project_id.as_deref() != Some(project) {
                    continue;
                }
            }
            active.push(agent_job_summary_value(&job));
        }

        active.sort_by(|a, b| {
            b["created_at"]
                .as_i64()
                .unwrap_or(0)
                .cmp(&a["created_at"].as_i64().unwrap_or(0))
        });
        let running_count = active
            .iter()
            .filter(|summary| {
                summary
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(is_blocking_active_job_status)
            })
            .count();
        // `recovering` jobs are a subset of running/blocking-active jobs that the
        // runner must reconcile before their output can be trusted. Counted over
        // the full active vector (not the truncated `recent` list) so the count is
        // reliable regardless of how many recent jobs are surfaced.
        let recovering_count = active
            .iter()
            .filter(|summary| summary.get("status").and_then(Value::as_str) == Some("recovering"))
            .count();
        let stop_requested_count = active
            .iter()
            .filter(|summary| {
                summary
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(is_stop_pending_job_status)
            })
            .count();
        let terminal_pending_count = stop_requested_count;
        let blocking_active_count = running_count;
        let nonblocking_active_count = terminal_pending_count;
        let active_count = blocking_active_count + nonblocking_active_count;
        let recent: Vec<Value> = active.iter().take(max).map(active_job_brief).collect();
        let mut warnings = Vec::new();
        if blocking_active_count > 0 {
            warnings.push(json!({
                "kind": "active_jobs_present",
                "blocking": true,
                "active_count": active_count,
                "blocking_active_count": blocking_active_count,
                "message": format!(
                    "{} blocking active job{} still running",
                    blocking_active_count,
                    if blocking_active_count == 1 { "" } else { "s" }
                ),
            }));
        }
        if terminal_pending_count > 0 {
            warnings.push(json!({
                "kind": "jobs_terminal_pending",
                "blocking": false,
                "stop_requested_count": stop_requested_count,
                "terminal_pending_count": terminal_pending_count,
                "message": format!(
                    "{} job{} stop_requested and waiting for terminal status",
                    terminal_pending_count,
                    if terminal_pending_count == 1 { " is" } else { "s are" }
                ),
            }));
        }
        json!({
            "active_count": active_count,
            "running_count": running_count,
            "recovering_count": recovering_count,
            "stop_requested_count": stop_requested_count,
            "terminal_pending_count": terminal_pending_count,
            "blocking_active_count": blocking_active_count,
            "nonblocking_active_count": nonblocking_active_count,
            "recent": recent,
            "recent_limit": max,
            "truncated": active_count > max,
            "warnings": warnings,
        })
    }

    /// Hidden REST compatibility wrapper for stopping a runtime Job by id.
    /// Registered Project Jobs are Runner-owned, so this delegates directly to
    /// the Runner Job registry and never attempts Server-local process control.
    pub async fn stop_job(&self, job_id: String) -> ToolResult {
        if !is_safe_job_id(&job_id) {
            return ToolResult::err("invalid job id");
        }
        match self
            .runner_registry
            .stop_job(&job_id, "runtime_http".to_string())
            .await
        {
            Ok(job) => ToolResult::ok(json!({
                "job_id": job.job_id,
                "project": job.project_id,
                "status": job.status,
            })),
            Err(error) if error.contains("unknown shell job") => {
                ToolResult::err(format!("unknown job: {job_id}"))
            }
            Err(error) => ToolResult::err(error),
        }
    }
}

#[cfg(test)]
mod recovery_projection_tests {
    use super::{
        confirmation_required_result, job_not_found_result, job_project_mismatch_result,
        job_recovering_stop_result, job_stop_forbidden_result, recovery_reason_text,
        validation_job_projection,
    };
    use crate::runner_protocol::ShellJobInfo;
    use serde_json::json;

    #[test]
    fn recovery_reason_text_recovering_explains_wait() {
        let text = recovery_reason_text(Some("recovering"), Some("runner_transport_disconnected"));
        assert_eq!(
            text.as_deref(),
            Some("server is waiting for the same runner instance to reconnect")
        );
        // recovering state is described regardless of the specific reason code.
        let text2 = recovery_reason_text(Some("recovering"), None);
        assert_eq!(
            text2.as_deref(),
            Some("server is waiting for the same runner instance to reconnect")
        );
    }

    #[test]
    fn recovery_reason_text_lost_after_reconcile_codes_are_distinct() {
        let deadline = recovery_reason_text(
            Some("lost_after_reconcile"),
            Some("runner_recovery_deadline_exceeded"),
        );
        let missing = recovery_reason_text(
            Some("lost_after_reconcile"),
            Some("runner_inventory_missing"),
        );
        let replaced = recovery_reason_text(
            Some("lost_after_reconcile"),
            Some("runner_instance_replaced"),
        );
        assert_eq!(
            deadline.as_deref(),
            Some("lost: runner did not reconnect before the recovery deadline")
        );
        assert_eq!(
            missing.as_deref(),
            Some("lost: runner reconnect did not report this job in its inventory")
        );
        assert_eq!(
            replaced.as_deref(),
            Some("lost: runner instance was replaced by a newer process")
        );
        // The three reasons must produce three distinct human strings so the
        // Console can tell them apart.
        let texts = [deadline, missing, replaced]
            .into_iter()
            .filter_map(|opt| opt.as_deref().map(str::to_string))
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(texts.len(), 3, "distinct recoverable loss reasons");
    }

    #[test]
    fn recovery_reason_text_runner_without_reconciliation_disconnect() {
        let text = recovery_reason_text(None, Some("runner_disconnected_without_reconciliation"));
        assert_eq!(
            text.as_deref(),
            Some("lost: runner disconnected without reconciliation support")
        );
    }

    #[test]
    fn recovery_reason_text_unknown_code_falls_back_safely() {
        let text = recovery_reason_text(Some("lost_after_reconcile"), Some("some_new_code"));
        assert!(
            text.as_deref().unwrap().contains("some_new_code"),
            "unknown code is echoed for debuggability"
        );
        assert!(
            !text.as_deref().unwrap().contains("token"),
            "no sensitive leak"
        );
        let other = recovery_reason_text(None, Some("unknown_reason"));
        assert!(other.as_deref().unwrap().contains("unknown_reason"));
    }

    #[test]
    fn recovery_reason_text_none_when_no_state_or_code() {
        assert_eq!(recovery_reason_text(None, None), None);
    }

    #[test]
    fn job_control_failures_expose_bounded_recovery_without_changing_lifecycle_fields() {
        let confirmation = confirmation_required_result("agent:special:demo", "job-1");
        assert_eq!(confirmation.output["error_kind"], "confirmation_required");
        assert_eq!(confirmation.output["failure_kind"], "confirmation_required");
        assert_eq!(confirmation.output["command_started"], false);
        assert_eq!(confirmation.output["recovery_kind"], "user_action");

        let missing = job_not_found_result("agent:special:demo", "job-missing");
        assert_eq!(missing.output["failure_kind"], "job_not_found");
        assert_eq!(missing.output["recovery_kind"], "reobserve");
        assert_eq!(missing.output["recovery_tool"], "list_jobs");

        let mismatch =
            job_project_mismatch_result("agent:special:demo", "agent:special:other", "job-2");
        assert_eq!(mismatch.output["failure_kind"], "job_project_mismatch");
        assert_eq!(mismatch.output["recovery_kind"], "fix_input");

        let forbidden = job_stop_forbidden_result(
            "agent:special:demo",
            "job-3",
            Some("wc_sess_request"),
            Some("wc_sess_owner"),
        );
        assert_eq!(forbidden.output["failure_kind"], "job_stop_forbidden");
        assert_eq!(forbidden.output["recovery_kind"], "fix_input");

        let recovering: ShellJobInfo = serde_json::from_value(json!({
            "job_id": "job-recovering",
            "client_id": "special",
            "command_preview": "",
            "status": "recovering",
            "created_at": 1,
            "recovery_state": "recovering",
            "recovery_reason_code": "runner_transport_disconnected"
        }))
        .unwrap();
        let wait = job_recovering_stop_result("agent:special:demo", &recovering);
        assert_eq!(wait.output["error_kind"], "runner_unavailable_recovering");
        assert_eq!(wait.output["failure_kind"], "runner_unavailable_recovering");
        assert_eq!(wait.output["command_started"], false);
        assert_eq!(wait.output["stop_effect"], "runner_unavailable");
        assert_eq!(wait.output["recovery_kind"], "wait");
        assert!(wait.output.get("recovery_tool").is_none());
    }

    #[test]
    fn validation_projection_reports_terminal_cargo_test_counts_and_diagnostics() {
        let success = validation_job_projection(
            Some("cargo_test"),
            Some("test"),
            "completed",
            Some(0),
            "running 3 tests\n\ntest result: ok. 3 passed; 0 failed; 0 ignored\n",
            "",
            false,
            None,
        )
        .unwrap();
        assert_eq!(success["passed"], true);
        assert_eq!(success["tests_detected"], true);
        assert_eq!(success["tests_run_count"], 3);
        assert_eq!(success["tests_passed"], 3);
        assert_eq!(success["tests_failed"], 0);
        assert_eq!(success["diagnostics"]["truncated"], false);

        let compile_error = validation_job_projection(
            Some("cargo_test"),
            Some("test"),
            "failed",
            Some(101),
            "",
            "error[E0308]: mismatched types\n --> src/lib.rs:1:1\n",
            false,
            None,
        )
        .unwrap();
        assert_eq!(compile_error["passed"], false);
        assert_eq!(compile_error["tests_detected"], false);
        assert!(compile_error["tests_run_count"].is_null());
        assert_eq!(compile_error["diagnostics"]["available"], true);
    }

    #[test]
    fn cargo_test_count_assertion_requires_complete_minimum_evidence() {
        let project = |stdout: &str, truncated: bool, minimum_tests: Option<u64>| {
            validation_job_projection(
                Some("cargo_test"),
                Some("test"),
                "completed",
                Some(0),
                stdout,
                "",
                truncated,
                minimum_tests,
            )
            .unwrap()
        };

        let ignored_only_default = project(
            "running 1 test\n\ntest ignored_only ... ignored\n\ntest result: ok. 0 passed; 0 failed; 1 ignored\n",
            false,
            None,
        );
        assert_eq!(ignored_only_default["passed"], true);
        assert_eq!(ignored_only_default["tests_run_count"], 0);
        assert_eq!(ignored_only_default["zero_tests_run"], true);
        assert!(ignored_only_default.get("test_count_assertion").is_none());

        let ignored_only_required = project(
            "running 1 test\n\ntest ignored_only ... ignored\n\ntest result: ok. 0 passed; 0 failed; 1 ignored\n",
            false,
            Some(1),
        );
        assert_eq!(ignored_only_required["passed"], false);
        assert_eq!(ignored_only_required["tests_run_count"], 0);
        assert_eq!(
            ignored_only_required["test_count_assertion"]["reason_code"],
            "minimum_not_met"
        );
        assert_eq!(
            ignored_only_required["test_count_assertion"]["evidence_reason_code"],
            "complete_summary"
        );

        let one_passed_with_ignored = project(
            "running 6 tests\n\ntest result: ok. 1 passed; 0 failed; 5 ignored\n",
            false,
            Some(1),
        );
        assert_eq!(one_passed_with_ignored["passed"], true);
        assert_eq!(one_passed_with_ignored["tests_run_count"], 1);
        assert_eq!(
            one_passed_with_ignored["test_count_assertion"]["reason_code"],
            "minimum_satisfied"
        );
        let one_passed_below_two = project(
            "running 6 tests\n\ntest result: ok. 1 passed; 0 failed; 5 ignored\n",
            false,
            Some(2),
        );
        assert_eq!(one_passed_below_two["passed"], false);
        assert_eq!(one_passed_below_two["tests_run_count"], 1);
        assert_eq!(
            one_passed_below_two["test_count_assertion"]["reason_code"],
            "minimum_not_met"
        );

        let compatible_zero = project(
            "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored\n",
            false,
            None,
        );
        assert_eq!(compatible_zero["passed"], true);
        assert_eq!(compatible_zero["tests_run_count"], 0);
        assert_eq!(compatible_zero["zero_tests_run"], true);
        assert!(compatible_zero.get("test_count_assertion").is_none());

        for (actual, minimum, status, passed) in [
            (0, 1, "failed", false),
            (5, 6, "failed", false),
            (6, 6, "passed", true),
            (10, 6, "passed", true),
        ] {
            let value = project(
                &format!(
                    "running {actual} tests\n\ntest result: ok. {actual} passed; 0 failed; 0 ignored\n"
                ),
                false,
                Some(minimum),
            );
            assert_eq!(value["passed"], passed);
            assert_eq!(value["test_count_assertion"]["minimum_tests"], minimum);
            assert_eq!(value["test_count_assertion"]["actual_tests_run"], actual);
            assert_eq!(value["test_count_assertion"]["status"], status);
            assert_eq!(
                value["test_count_assertion"]["evidence_reason_code"],
                "complete_summary"
            );
            assert_eq!(
                value["test_count_assertion"]["reason_code"],
                if passed {
                    "minimum_satisfied"
                } else {
                    "minimum_not_met"
                }
            );
        }

        for unproven in [
            project("", false, Some(1)),
            project("running 10 tests\n", false, Some(6)),
            project("test result: ok. 10 passed;\n", false, Some(6)),
            project("test result: ok. 10 passed; 0 failed\n", true, Some(6)),
        ] {
            assert_eq!(unproven["passed"], false);
            assert!(unproven["tests_run_count"].is_null());
            assert_eq!(unproven["test_count_assertion"]["status"], "unproven");
            assert_eq!(
                unproven["test_count_assertion"]["reason_code"],
                "test_count_unproven"
            );
            assert!(unproven["test_count_assertion"]["actual_tests_run"].is_null());
        }

        assert_eq!(
            project("", false, Some(1))["test_count_assertion"]["evidence_reason_code"],
            "no_complete_summary"
        );
        assert_eq!(
            project("test result: ok. 10 passed;\n", false, Some(6))["test_count_assertion"]
                ["evidence_reason_code"],
            "partial_harness_summary"
        );
        assert_eq!(
            project(
                "test result: ok. 10 passed; 0 failed; 0 ignored\n",
                true,
                Some(6)
            )["test_count_assertion"]["evidence_reason_code"],
            "output_truncated"
        );

        let command_failure = validation_job_projection(
            Some("cargo_test"),
            Some("test"),
            "failed",
            Some(101),
            "",
            "error: could not compile",
            false,
            Some(6),
        )
        .unwrap();
        assert_eq!(command_failure["passed"], false);
        assert!(
            command_failure.get("test_count_assertion").is_none(),
            "a real Cargo failure must not be overwritten by postcondition evaluation"
        );

        let failing_tests = validation_job_projection(
            Some("cargo_test"),
            Some("test"),
            "failed",
            Some(101),
            "running 3 tests\n\ntest result: FAILED. 2 passed; 1 failed; 0 ignored\n",
            "",
            false,
            Some(1),
        )
        .unwrap();
        assert_eq!(failing_tests["passed"], false);
        assert_eq!(failing_tests["tests_run_count"], 3);
        assert_eq!(failing_tests["tests_passed"], 2);
        assert_eq!(failing_tests["tests_failed"], 1);
        assert!(
            failing_tests.get("test_count_assertion").is_none(),
            "executed-test evidence must not overwrite Cargo process failure"
        );
    }

    #[test]
    fn validation_projection_reports_check_counts_and_never_fakes_truncated_counts() {
        let complete = validation_job_projection(
            Some("cargo_check"),
            Some("check"),
            "failed",
            Some(101),
            "",
            "warning: unused import\nerror[E0308]: mismatched types\n",
            false,
            None,
        )
        .unwrap();
        assert_eq!(complete["warnings_count"], 1);
        assert_eq!(complete["errors_count"], 1);

        let truncated = validation_job_projection(
            Some("cargo_check"),
            Some("check"),
            "failed",
            Some(101),
            "",
            "error[E0308]: mismatched types\n",
            true,
            None,
        )
        .unwrap();
        assert!(truncated["warnings_count"].is_null());
        assert!(truncated["errors_count"].is_null());
        assert_eq!(truncated["diagnostics"]["truncated"], true);
    }

    #[test]
    fn validation_projection_keeps_lifecycle_terminals_distinct_from_validation_failure() {
        let fmt = validation_job_projection(
            Some("cargo_fmt"),
            Some("format"),
            "failed",
            Some(1),
            "Diff in src/lib.rs\n",
            "",
            false,
            None,
        )
        .unwrap();
        assert_eq!(fmt["passed"], false);
        assert!(fmt.get("diagnostics").is_none());

        for (status, state) in [
            ("timeout", "timed_out"),
            ("stopped", "cancelled"),
            ("lost", "lost"),
        ] {
            let value = validation_job_projection(
                Some("cargo_test"),
                Some("test"),
                status,
                None,
                "running 1 test\n",
                "",
                false,
                None,
            )
            .unwrap();
            assert_eq!(value["state"], state);
            assert!(value["passed"].is_null());
            assert!(value["tests_run_count"].is_null());
            assert!(value["tests_passed"].is_null());
            assert!(value["tests_failed"].is_null());
        }
    }

    #[test]
    fn agent_job_summary_hides_internal_validation_correlation_metadata() {
        use crate::runner_protocol::{ShellJobInfo, ShellJobStructuredExecutionMetadata};
        let job = ShellJobInfo {
            job_id: "job-validation-summary".to_string(),
            request_id: Some("req-validation-summary".to_string()),
            client_id: "oe".to_string(),
            kind: "run_process".to_string(),
            project_id: Some("agent:demo".to_string()),
            session_id: Some("wc_sess_demo".to_string()),
            ssh_resource: None,
            cwd: Some(".".to_string()),
            project_cwd: Some(".".to_string()),
            purpose: Some("test".to_string()),
            shell: Some("direct_argv".to_string()),
            command_preview: "validation".to_string(),
            status: "running".to_string(),
            created_at: 1,
            started_at: Some(1),
            ended_at: None,
            exit_code: None,
            duration_ms: None,
            elapsed_secs: Some(0),
            error: None,
            command_execution_state: None,
            structured_execution: Some(ShellJobStructuredExecutionMetadata {
                execution_source: "run_process".to_string(),
                language: None,
                script_bytes: None,
                arg_count: 2,
                stdin_present: false,
                validation_identity: Some("assertion:0123456789abcdef01234567".to_string()),
                assertion_name: Some("Bearer historical-validation-secret".to_string()),
                validation_tool: Some("cargo_test".to_string()),
            }),
            codex: None,
            result: None,
            validation_progress: None,
            activity: None,
            validation: None,
            recovery_state: None,
            recovered_after_server_restart: false,
            reconciled_at: None,
            recovery_reason_code: None,
            observation_token: None,
            last_update_seq: None,
            stdout_retained_from_line: Some(1),
            stderr_retained_from_line: Some(1),
            stdout_log_truncated: false,
            stderr_log_truncated: false,
        };
        let value = super::agent_job_summary_value(&job);
        let structured = &value["structured_execution"];
        assert_eq!(structured["execution_source"], "run_process");
        assert!(structured.get("validation_identity").is_none());
        assert!(structured.get("validation_tool").is_none());
        assert!(structured.get("assertion_name").is_none());
        assert!(!value.to_string().contains("historical-validation-secret"));
    }

    #[test]
    fn agent_job_summary_includes_recovery_reason() {
        use crate::runner_protocol::ShellJobInfo;
        let job = ShellJobInfo {
            job_id: "job-1".to_string(),
            request_id: Some("req-1".to_string()),
            client_id: "oe".to_string(),
            kind: "shell".to_string(),
            project_id: None,
            session_id: None,
            ssh_resource: None,
            cwd: None,
            project_cwd: None,
            purpose: None,
            shell: None,
            command_preview: String::new(),
            status: "lost".to_string(),
            created_at: 1,
            started_at: Some(2),
            ended_at: Some(3),
            exit_code: None,
            duration_ms: None,
            elapsed_secs: Some(1),
            error: Some("runner did not reconcile".to_string()),
            command_execution_state: None,
            structured_execution: None,
            codex: None,
            result: None,
            validation_progress: None,
            activity: None,
            validation: None,
            recovery_state: Some("lost_after_reconcile".to_string()),
            recovered_after_server_restart: true,
            reconciled_at: Some(3),
            recovery_reason_code: Some("runner_recovery_deadline_exceeded".to_string()),
            observation_token: Some("wjob1:a:job-1:0123456789abcdef:4".to_string()),
            last_update_seq: Some(4),
            stdout_retained_from_line: Some(1),
            stderr_retained_from_line: Some(1),
            stdout_log_truncated: false,
            stderr_log_truncated: false,
        };
        let value = super::agent_job_summary_value(&job);
        assert_eq!(
            value["recovery_reason_code"],
            json!("runner_recovery_deadline_exceeded")
        );
        assert_eq!(
            value["recovery_reason"],
            json!("lost: runner did not reconnect before the recovery deadline")
        );
        // Never expose the raw error string or command payload via the summary.
        assert!(
            value.get("error").is_none(),
            "summary must not surface raw error"
        );
        assert!(
            value.get("command").is_none(),
            "summary must not surface command"
        );
    }
}
