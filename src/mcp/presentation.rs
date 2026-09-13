use serde_json::{json, Map, Value};
use webcodex_core::runner_job_lifecycle::RunnerJobLifecycle;

pub(super) const MCP_PRESENTATION_META_KEY: &str = "webcodex/presentation";
pub(super) const MCP_PRESENTATION_VERSION: u64 = 1;
pub(super) const MAX_MCP_PRESENTATION_ITEMS: usize = 8;
pub(super) const MAX_MCP_PRESENTATION_TEXT_CHARS: usize = 256;
pub(super) const MAX_MCP_PRESENTATION_DIFF_HUNKS: usize = 4;
pub(super) const MAX_MCP_PRESENTATION_DIFF_LINES: usize = 80;
pub(super) const MAX_MCP_PRESENTATION_DIFF_CHARS: usize = 12 * 1024;

/// Static MCP App descriptor eligibility only. One advertised App binding creates
/// one extra Host presentation per tool result, so keep this deliberately sparse:
/// routine execution/observation stays on the Host's native tool card.
pub(super) fn tool_supports_result_app(tool_name: &str) -> bool {
    tool_name == "show_changes"
}

/// Dedicated sparse Goal Plan App binding. Only the explicit presentation entry
/// gets a resource; Goal mutations, execution tools, and app-only polling never
/// create additional Host cards.
pub(super) fn tool_supports_goal_plan_app(tool_name: &str) -> bool {
    tool_name == "present_goal_plan"
}

/// Dedicated Durable Agent continuation controller App binding. Only the
/// explicit presentation entry is model-visible and creates the persistent card.
/// App-only coordination tools may still declare the same resource association
/// as a Host compatibility hint without exposing them to the model or granting authority.
pub(super) fn tool_supports_agent_continuation_app(tool_name: &str) -> bool {
    tool_name == "present_agent_continuation"
}

/// Bounded presentation projections retained for current milestone cards and for
/// already-cached older tool descriptors. Projection support does not itself bind
/// a new App card in tools/list.
fn tool_has_result_presentation_projection(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "list_jobs"
            | "observe_jobs"
            | "cargo_check"
            | "cargo_test"
            | "go_test"
            | "validation_summary"
            | "show_changes"
            | "git_review_summary"
    )
}

fn validation_kind_for_tool(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        "cargo_check" => Some("check"),
        "cargo_test" | "go_test" => Some("test"),
        _ => None,
    }
}

fn bounded_text(value: &Value) -> Option<String> {
    let value = value.as_str()?;
    let mut chars = value.chars();
    let bounded = chars
        .by_ref()
        .take(MAX_MCP_PRESENTATION_TEXT_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        let mut truncated = bounded
            .chars()
            .take(MAX_MCP_PRESENTATION_TEXT_CHARS.saturating_sub(1))
            .collect::<String>();
        truncated.push('…');
        Some(truncated)
    } else {
        Some(bounded)
    }
}

fn copy_bounded_text(source: &Value, target: &mut Map<String, Value>, key: &str) {
    if let Some(value) = source.get(key).and_then(bounded_text) {
        target.insert(key.to_string(), Value::String(value));
    }
}

fn copy_scalar(source: &Value, target: &mut Map<String, Value>, key: &str) {
    if let Some(value) = source.get(key) {
        if value.is_boolean() || value.is_number() {
            target.insert(key.to_string(), value.clone());
        }
    }
}

fn safe_job_progress(state: &str, reason_code: &str) -> Option<Value> {
    if !matches!(state, "working" | "waiting") {
        return None;
    }
    let summary = match reason_code {
        "process_running" => "Process execution in progress",
        "validation_format" => "Formatting validation in progress",
        "validation_check" => "Check validation in progress",
        "validation_test" => "Test validation in progress",
        "cargo_waiting_for_build_lock" | "cargo_build_lock" => "Waiting for Cargo build lock",
        "cargo_compiling" => "Cargo compilation in progress",
        "cargo_checking" => "Cargo checking in progress",
        _ => return None,
    };
    Some(json!({
        "state": state,
        "reason_code": reason_code,
        "summary": summary,
    }))
}

fn job_progress_presentation(source: &Value) -> Option<Value> {
    if let Some(progress) = source
        .pointer("/detected_summary/progress")
        .and_then(Value::as_object)
    {
        if let (Some(state), Some(reason_code)) = (
            progress.get("state").and_then(Value::as_str),
            progress.get("reason_code").and_then(Value::as_str),
        ) {
            if let Some(projected) = safe_job_progress(state, reason_code) {
                return Some(projected);
            }
        }
    }

    let activity = source.get("activity")?.as_object()?;
    let state = activity.get("state")?.as_str()?;
    let reason_code = activity.get("phase")?.as_str()?;
    safe_job_progress(state, reason_code)
}

fn job_work_presentation(source: &Value) -> Option<Value> {
    let detected = source.get("detected_summary")?.as_object()?;
    let mut output = Map::new();
    if let Some(kind) = detected.get("kind").and_then(Value::as_str).filter(|kind| {
        matches!(
            *kind,
            "test"
                | "check"
                | "format"
                | "build"
                | "validation"
                | "release"
                | "diagnostic"
                | "operation"
        )
    }) {
        output.insert("kind".to_string(), Value::String(kind.to_string()));
    }
    if let Some(outcome) = detected
        .get("outcome")
        .and_then(Value::as_str)
        .filter(|outcome| {
            matches!(
                *outcome,
                "in_progress" | "passed" | "failed" | "timed_out" | "cancelled"
            )
        })
    {
        output.insert("outcome".to_string(), Value::String(outcome.to_string()));
    }
    let detected = Value::Object(detected.clone());
    for key in [
        "tests_detected",
        "tests_run_count",
        "zero_tests_run",
        "tests_passed",
        "tests_failed",
    ] {
        copy_scalar(&detected, &mut output, key);
    }
    (!output.is_empty()).then(|| Value::Object(output))
}

fn apply_job_lifecycle(item: &mut Map<String, Value>, status: &str) {
    let active = webcodex_runner_registry::job_status_is_active(status);
    let lifecycle = RunnerJobLifecycle::from_wire(status).ok();
    let terminal_pending = lifecycle == Some(RunnerJobLifecycle::StopRequested);
    let terminal = lifecycle.is_some_and(RunnerJobLifecycle::is_terminal);
    item.insert("active".to_string(), Value::Bool(active));
    item.insert("terminal".to_string(), Value::Bool(terminal));
    item.insert(
        "terminal_pending".to_string(),
        Value::Bool(terminal_pending),
    );
    item.insert(
        "blocking_active".to_string(),
        Value::Bool(active && !terminal_pending),
    );
}

fn job_item_needs_attention(item: &Value) -> bool {
    matches!(
        item.get("status").and_then(Value::as_str),
        Some("failed" | "lost" | "timeout" | "timed_out")
    ) || matches!(
        item.get("recovery_state").and_then(Value::as_str),
        Some("recovering" | "lost_after_reconcile")
    ) || item.get("command_execution_state").and_then(Value::as_str) == Some("outcome_unknown")
        || item.get("error_kind").is_some()
}

fn static_guidance(
    status: Option<&str>,
    execution_state: Option<&str>,
    recovery: Option<&str>,
) -> Option<&'static str> {
    if execution_state == Some("outcome_unknown") {
        return Some("Outcome is uncertain. Observe current Job state before retrying.");
    }
    if status == Some("lost") {
        return Some(
            "Job state is lost. Re-observe runtime state before deciding whether retry is safe.",
        );
    }
    match recovery {
        Some("recovering") => {
            Some("Job recovery is in progress; this card does not poll or retry automatically.")
        }
        Some("lost_after_reconcile") => Some(
            "Job remained lost after reconciliation; inspect current runtime state before retrying.",
        ),
        _ => None,
    }
}

fn job_summary_presentation(job: &Value) -> Option<Value> {
    let job_id = job.get("job_id").and_then(bounded_text)?;
    let status = job.get("status").and_then(bounded_text)?;
    let mut item = Map::new();
    item.insert("job_id".to_string(), Value::String(job_id));
    item.insert("status".to_string(), Value::String(status.clone()));
    copy_bounded_text(job, &mut item, "project");
    copy_bounded_text(job, &mut item, "command_execution_state");
    copy_bounded_text(job, &mut item, "recovery_state");
    copy_bounded_text(job, &mut item, "recovery_reason_code");
    copy_bounded_text(job, &mut item, "recovery_reason");
    for key in ["duration_ms", "elapsed_secs", "exit_code"] {
        copy_scalar(job, &mut item, key);
    }
    apply_job_lifecycle(&mut item, &status);
    if let Some(progress) = job_progress_presentation(job) {
        item.insert("progress".to_string(), progress);
    }
    if let Some(work) = job_work_presentation(job) {
        item.insert("work".to_string(), work);
    }
    let execution_state = job.get("command_execution_state").and_then(Value::as_str);
    let recovery_state = job.get("recovery_state").and_then(Value::as_str);
    if let Some(guidance) = static_guidance(Some(&status), execution_state, recovery_state) {
        item.insert("guidance".to_string(), Value::String(guidance.to_string()));
    }
    Some(Value::Object(item))
}

fn list_jobs_presentation(output: &Value) -> Option<Value> {
    let jobs = output.get("jobs")?.as_array()?;
    let projected = jobs
        .iter()
        .filter_map(job_summary_presentation)
        .collect::<Vec<_>>();
    let foreground_count = projected
        .iter()
        .filter(|item| {
            item.get("active").and_then(Value::as_bool) == Some(true)
                || job_item_needs_attention(item)
        })
        .count();
    let routine_omitted_count = projected.len().saturating_sub(foreground_count);
    let items = projected
        .into_iter()
        .filter(|item| {
            item.get("active").and_then(Value::as_bool) == Some(true)
                || job_item_needs_attention(item)
        })
        .take(MAX_MCP_PRESENTATION_ITEMS)
        .collect::<Vec<_>>();
    let shown_active_count = items
        .iter()
        .filter(|item| item.get("active").and_then(Value::as_bool) == Some(true))
        .count();
    let shown_terminal_count = items
        .iter()
        .filter(|item| item.get("terminal").and_then(Value::as_bool) == Some(true))
        .count();
    let shown_attention_count = items
        .iter()
        .filter(|item| job_item_needs_attention(item))
        .count();
    Some(json!({
        "version": MCP_PRESENTATION_VERSION,
        "kind": "job_list",
        "count": output.get("count").and_then(Value::as_u64)?,
        "matched_count": output.get("matched_count").and_then(Value::as_u64)?,
        "truncated": output.get("truncated").and_then(Value::as_bool)?,
        "presented_count": items.len(),
        "routine_omitted_count": routine_omitted_count,
        "items_truncated": foreground_count > MAX_MCP_PRESENTATION_ITEMS,
        "shown_active_count": shown_active_count,
        "shown_terminal_count": shown_terminal_count,
        "shown_attention_count": shown_attention_count,
        "items": items,
    }))
}

fn observation_guidance(item: &Value) -> Option<&'static str> {
    let status = item.get("status").and_then(Value::as_str);
    let execution_state = item.get("command_execution_state").and_then(Value::as_str);
    let recovery_state = item.get("recovery_state").and_then(Value::as_str);
    static_guidance(status, execution_state, recovery_state)
}

fn observed_success_presentation(observation: &Value) -> Option<Value> {
    let job_id = observation.get("job_id").and_then(bounded_text)?;
    let status = observation.get("status").and_then(bounded_text)?;
    let mut item = Map::new();
    item.insert("job_id".to_string(), Value::String(job_id));
    item.insert("status".to_string(), Value::String(status.clone()));
    for key in [
        "command_execution_state",
        "recovery_state",
        "recovery_reason_code",
        "recovery_reason",
        "log_delta_status",
    ] {
        copy_bounded_text(observation, &mut item, key);
    }
    for key in [
        "changed",
        "exit_code",
        "stdout_lines",
        "stderr_lines",
        "stdout_returned_lines",
        "stderr_returned_lines",
        "stdout_truncated",
        "stderr_truncated",
        "stdout_delta_reset",
        "stderr_delta_reset",
        "earlier_stdout_unavailable",
        "earlier_stderr_unavailable",
    ] {
        copy_scalar(observation, &mut item, key);
    }
    apply_job_lifecycle(&mut item, &status);
    if let Some(progress) = job_progress_presentation(observation) {
        item.insert("progress".to_string(), progress);
    }
    if let Some(work) = job_work_presentation(observation) {
        item.insert("work".to_string(), work);
    }
    if let Some(guidance) = observation_guidance(observation) {
        item.insert("guidance".to_string(), Value::String(guidance.to_string()));
    }
    Some(Value::Object(item))
}

fn observed_failure_presentation(item: &Value) -> Option<Value> {
    let job_id = item.get("job_id").and_then(bounded_text)?;
    let mut output = Map::new();
    output.insert("job_id".to_string(), Value::String(job_id));
    for key in ["error_kind", "recovery_kind", "recovery_tool"] {
        copy_bounded_text(item, &mut output, key);
    }
    if item.get("error_kind").and_then(Value::as_str) == Some("unknown_job") {
        output.insert(
            "guidance".to_string(),
            Value::String(
                "Job is not directly observable here. Re-observe caller-visible Jobs before retrying."
                    .to_string(),
            ),
        );
    }
    Some(Value::Object(output))
}

fn observe_jobs_presentation(output: &Value) -> Option<Value> {
    let source_items = output.get("items")?.as_array()?;
    let items = source_items
        .iter()
        .take(MAX_MCP_PRESENTATION_ITEMS)
        .filter_map(|item| {
            if item.get("success").is_some() {
                if item.get("success").and_then(Value::as_bool) == Some(true) {
                    item.get("output").and_then(observed_success_presentation)
                } else {
                    observed_failure_presentation(item)
                }
            } else {
                observed_success_presentation(item)
            }
        })
        .collect::<Vec<_>>();

    let mut presentation = Map::new();
    presentation.insert("version".to_string(), Value::from(MCP_PRESENTATION_VERSION));
    presentation.insert(
        "kind".to_string(),
        Value::String("job_observation".to_string()),
    );
    presentation.insert("items".to_string(), Value::Array(items));
    presentation.insert(
        "items_truncated".to_string(),
        Value::Bool(source_items.len() > MAX_MCP_PRESENTATION_ITEMS),
    );
    if let Some(wait) = output.get("wait").and_then(Value::as_object) {
        let mut bounded_wait = Map::new();
        if let Some(outcome) = wait.get("outcome").and_then(bounded_text) {
            bounded_wait.insert("outcome".to_string(), Value::String(outcome));
        }
        if let Some(waited_ms) = wait.get("waited_ms").filter(|value| value.is_number()) {
            bounded_wait.insert("waited_ms".to_string(), waited_ms.clone());
        }
        if !bounded_wait.is_empty() {
            presentation.insert("wait".to_string(), Value::Object(bounded_wait));
        }
    }
    for key in [
        "requested_count",
        "returned_count",
        "succeeded_count",
        "failed_count",
        "changed_count",
        "terminal_count",
        "output_truncated",
        "next_index",
    ] {
        if let Some(value) = output.get(key) {
            if value.is_boolean() || value.is_number() || value.is_null() {
                presentation.insert(key.to_string(), value.clone());
            }
        }
    }
    Some(Value::Object(presentation))
}

fn validation_diagnostic_item(value: &Value) -> Option<Value> {
    let severity = value.get("severity").and_then(bounded_text)?;
    let message = value.get("message").and_then(bounded_text)?;
    let mut item = Map::new();
    item.insert("severity".to_string(), Value::String(severity));
    copy_bounded_text(value, &mut item, "code");
    item.insert("message".to_string(), Value::String(message));
    Some(Value::Object(item))
}

fn validation_failed_test_item(value: &Value) -> Option<Value> {
    let name = value.get("name").and_then(bounded_text)?;
    let failure_kind = value.get("failure_kind").and_then(bounded_text)?;
    Some(json!({
        "name": name,
        "failure_kind": failure_kind,
    }))
}

fn validation_diagnostics_presentation(diagnostics: &Value) -> Option<Value> {
    diagnostics.as_object()?;
    let mut output = Map::new();
    for key in [
        "available",
        "diagnostic_count",
        "returned_diagnostic_count",
        "diagnostics_truncated",
        "failed_test_details_truncated",
    ] {
        copy_scalar(diagnostics, &mut output, key);
    }

    let diagnostic_source = diagnostics
        .get("diagnostics")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let failed_source = diagnostics
        .get("failed_test_details")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let source_items = diagnostic_source.len().saturating_add(failed_source.len());
    let mut remaining = MAX_MCP_PRESENTATION_ITEMS;
    let mut diagnostic_items = Vec::new();
    for item in diagnostic_source {
        if remaining == 0 {
            break;
        }
        if let Some(item) = validation_diagnostic_item(item) {
            diagnostic_items.push(item);
            remaining -= 1;
        }
    }
    let mut failed_tests = Vec::new();
    for item in failed_source {
        if remaining == 0 {
            break;
        }
        if let Some(item) = validation_failed_test_item(item) {
            failed_tests.push(item);
            remaining -= 1;
        }
    }
    if !diagnostic_items.is_empty() {
        output.insert("items".to_string(), Value::Array(diagnostic_items));
    }
    if !failed_tests.is_empty() {
        output.insert("failed_tests".to_string(), Value::Array(failed_tests));
    }
    output.insert(
        "presentation_items_truncated".to_string(),
        Value::Bool(source_items > MAX_MCP_PRESENTATION_ITEMS),
    );
    Some(Value::Object(output))
}

fn validation_run_presentation(tool_name: &str, output: &Value) -> Option<Value> {
    output.as_object()?;
    let validation_kind = validation_kind_for_tool(tool_name)?;
    let mut presentation = Map::new();
    presentation.insert("version".to_string(), Value::from(MCP_PRESENTATION_VERSION));
    presentation.insert(
        "kind".to_string(),
        Value::String("validation_run".to_string()),
    );
    presentation.insert("tool".to_string(), Value::String(tool_name.to_string()));
    presentation.insert(
        "validation_kind".to_string(),
        Value::String(validation_kind.to_string()),
    );
    for key in ["execution_state", "failure_kind", "job_id", "job_status"] {
        copy_bounded_text(output, &mut presentation, key);
    }
    for key in [
        "terminal",
        "passed",
        "command_started",
        "command_completed",
        "duration_ms",
        "exit_code",
        "promoted_to_job",
        "warnings_count",
        "errors_count",
        "tests_detected",
        "tests_run_count",
        "tests_passed",
        "tests_failed",
        "zero_tests_run",
    ] {
        copy_scalar(output, &mut presentation, key);
    }
    if let Some(diagnostics) = output
        .get("diagnostics")
        .and_then(validation_diagnostics_presentation)
    {
        presentation.insert("diagnostics".to_string(), diagnostics);
    }
    Some(Value::Object(presentation))
}

fn validation_event_presentation(event: &Value) -> Option<Value> {
    event.as_object()?;
    let mut item = Map::new();
    for key in [
        "tool_name",
        "validation_kind",
        "failure_class",
        "failure_kind",
        "summary",
    ] {
        copy_bounded_text(event, &mut item, key);
    }
    for key in [
        "success",
        "validation_passed",
        "execution_success",
        "expectation_satisfied",
        "unresolved_failure",
        "duration_ms",
        "tests_detected",
        "tests_run_count",
        "zero_tests_run",
    ] {
        copy_scalar(event, &mut item, key);
    }
    if let Some(test_summary) = event.pointer("/diagnostics/test_summary") {
        if let Some(value) = test_summary.get("passed").filter(|value| value.is_number()) {
            item.insert("tests_passed".to_string(), value.clone());
        }
        if let Some(value) = test_summary.get("failed").filter(|value| value.is_number()) {
            item.insert("tests_failed".to_string(), value.clone());
        }
    }
    (!item.is_empty()).then_some(Value::Object(item))
}

fn validation_current_evidence_presentation(current: &Value) -> Option<Value> {
    current.as_object()?;
    let mut output = Map::new();
    for key in ["status", "reason", "latest_status", "boundary_reason"] {
        copy_bounded_text(current, &mut output, key);
    }
    for key in [
        "events_total",
        "successes",
        "failures",
        "expected_results",
        "resolved_failure_count",
        "unresolved_failure_count",
        "evidence_gap_event_count",
        "stale_failure_count",
        "evidence_after_latest_content_change",
    ] {
        copy_scalar(current, &mut output, key);
    }
    (!output.is_empty()).then_some(Value::Object(output))
}

fn validation_failure_set_count(validation: &Value, key: &str) -> Option<Value> {
    let set = validation.get(key)?;
    let count = set.get("count").filter(|value| value.is_number())?;
    Some(json!({"count": count.clone()}))
}

fn validation_historical_failures_presentation(validation: &Value) -> Option<Value> {
    let history = validation.get("historical_failures")?;
    history.as_object()?;
    let mut output = Map::new();
    for key in ["count", "resolved", "unresolved"] {
        copy_scalar(history, &mut output, key);
    }
    (!output.is_empty()).then_some(Value::Object(output))
}

fn validation_summary_presentation(output: &Value) -> Option<Value> {
    let validation = output.get("validation")?;
    validation.as_object()?;
    let mut summary = Map::new();
    for key in ["status", "latest_status", "reason"] {
        copy_bounded_text(validation, &mut summary, key);
    }
    for key in [
        "available",
        "events_total",
        "successes",
        "failures",
        "expected_results",
        "cargo_test_zero_tests_run",
    ] {
        copy_scalar(validation, &mut summary, key);
    }
    if let Some(current) = validation
        .get("current_evidence")
        .and_then(validation_current_evidence_presentation)
    {
        summary.insert("current_evidence".to_string(), current);
    }
    if let Some(history) = validation_historical_failures_presentation(validation) {
        summary.insert("historical_failures".to_string(), history);
    }
    for key in ["resolved_failures", "unresolved_failures", "evidence_gaps"] {
        if let Some(count) = validation_failure_set_count(validation, key) {
            summary.insert(key.to_string(), count);
        }
    }

    if let Some(source_events) = validation.get("events").and_then(Value::as_array) {
        // Canonical validation events are chronological. Keep the most recent
        // bounded subset without changing their canonical relative ordering.
        let skip = source_events
            .len()
            .saturating_sub(MAX_MCP_PRESENTATION_ITEMS);
        let events = source_events
            .iter()
            .skip(skip)
            .filter_map(validation_event_presentation)
            .collect::<Vec<_>>();
        summary.insert("events".to_string(), Value::Array(events));
        summary.insert(
            "events_truncated".to_string(),
            Value::Bool(source_events.len() > MAX_MCP_PRESENTATION_ITEMS),
        );
    }
    Some(json!({
        "version": MCP_PRESENTATION_VERSION,
        "kind": "validation_summary",
        "validation": Value::Object(summary),
    }))
}

fn safe_label(value: &Value) -> Option<String> {
    let value = value.as_str()?;
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '_' | '-'))
    {
        return None;
    }
    bounded_text(&Value::String(value.to_string()))
}

fn safe_repo_relative_path(value: &Value) -> Option<String> {
    let path = value.as_str()?;
    if path.is_empty()
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains("://")
        || path.chars().any(char::is_control)
        || path
            .split(|ch| ch == '/' || ch == '\\')
            .any(|component| component == "..")
    {
        return None;
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
    {
        return None;
    }
    bounded_text(value)
}

fn short_git_commit(value: &Value) -> Option<String> {
    let value = value.as_str()?;
    if !(4..=40).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(value[..value.len().min(8)].to_ascii_lowercase())
}

fn bounded_safe_labels(source: &Value) -> (Vec<Value>, bool) {
    let Some(source) = source.as_array() else {
        return (Vec::new(), false);
    };
    let mut values = Vec::new();
    let mut truncated = false;
    for value in source {
        if values.len() == MAX_MCP_PRESENTATION_ITEMS {
            truncated = true;
            break;
        }
        if let Some(value) = safe_label(value) {
            values.push(Value::String(value));
        } else {
            truncated = true;
        }
    }
    (
        values,
        truncated || source.len() > MAX_MCP_PRESENTATION_ITEMS,
    )
}

fn bounded_diff_text(value: &Value) -> Option<(String, bool)> {
    let value = value.as_str()?;
    if value
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return None;
    }
    let source_lines = value.lines().collect::<Vec<_>>();
    let mut truncated = source_lines.len() > MAX_MCP_PRESENTATION_DIFF_LINES;
    let line_bounded = source_lines
        .into_iter()
        .take(MAX_MCP_PRESENTATION_DIFF_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    let mut chars = line_bounded.chars();
    let bounded = chars
        .by_ref()
        .take(MAX_MCP_PRESENTATION_DIFF_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        truncated = true;
    }
    Some((bounded, truncated))
}

fn show_changes_diff_hunks_for_path(
    output: &Value,
    path: &str,
    remaining_hunks: &mut usize,
) -> (Vec<Value>, bool) {
    let Some(files) = output.get("hunks").and_then(Value::as_array) else {
        return (Vec::new(), false);
    };
    let mut result = Vec::new();
    let mut truncated = false;
    for file in files {
        let Some(file_path) = file.get("path").and_then(safe_repo_relative_path) else {
            continue;
        };
        if file_path != path {
            continue;
        }
        let Some(hunks) = file.get("hunks").and_then(Value::as_array) else {
            continue;
        };
        for hunk in hunks {
            if *remaining_hunks == 0 {
                truncated = true;
                break;
            }
            let Some((diff, diff_truncated)) = hunk.get("diff").and_then(bounded_diff_text) else {
                truncated = true;
                continue;
            };
            let hunk_truncated =
                diff_truncated || hunk.get("truncated").and_then(Value::as_bool) == Some(true);
            if hunk_truncated {
                truncated = true;
            }
            let mut projected = Map::new();
            projected.insert("diff".to_string(), Value::String(diff));
            projected.insert("truncated".to_string(), Value::Bool(hunk_truncated));
            result.push(Value::Object(projected));
            *remaining_hunks -= 1;
        }
        break;
    }
    (result, truncated)
}

fn show_changes_file_presentation(
    file: &Value,
    output: &Value,
    remaining_hunks: &mut usize,
) -> Option<Value> {
    file.as_object()?;
    let path = file.get("path").and_then(safe_repo_relative_path)?;
    let mut item = Map::new();
    item.insert("path".to_string(), Value::String(path.clone()));
    if let Some(status) = file.get("status").and_then(safe_label) {
        item.insert("status".to_string(), Value::String(status));
    }
    if let Some(kind) = file.get("kind").and_then(safe_label) {
        item.insert("kind".to_string(), Value::String(kind));
    }
    for key in ["staged", "unstaged", "additions", "deletions"] {
        copy_scalar(file, &mut item, key);
    }
    if let Some(old_path) = file.get("old_path").and_then(safe_repo_relative_path) {
        item.insert("old_path".to_string(), Value::String(old_path));
    }
    let (diff_hunks, diff_truncated) =
        show_changes_diff_hunks_for_path(output, &path, remaining_hunks);
    if !diff_hunks.is_empty() {
        item.insert("diff_hunks".to_string(), Value::Array(diff_hunks));
    }
    if diff_truncated {
        item.insert("diff_truncated".to_string(), Value::Bool(true));
    }
    Some(Value::Object(item))
}

fn show_changes_status_observation(output: &Value) -> Option<Value> {
    let observation = output.get("status_observation")?;
    observation.as_object()?;
    let mut result = Map::new();
    for key in ["status", "reason_code"] {
        if let Some(value) = observation.get(key).and_then(safe_label) {
            result.insert(key.to_string(), Value::String(value));
        }
    }
    copy_scalar(observation, &mut result, "exit_code");
    (!result.is_empty()).then_some(Value::Object(result))
}

fn show_changes_presentation(output: &Value) -> Option<Value> {
    output.as_object()?;
    let mut presentation = Map::new();
    presentation.insert("version".to_string(), Value::from(MCP_PRESENTATION_VERSION));
    presentation.insert("kind".to_string(), Value::String("git_changes".to_string()));
    for key in ["branch"] {
        copy_bounded_text(output, &mut presentation, key);
    }
    for key in ["upstream_status", "upstream_reason_code"] {
        if let Some(value) = output.get(key).and_then(safe_label) {
            presentation.insert(key.to_string(), Value::String(value));
        }
    }
    for key in [
        "git_available",
        "non_git_project",
        "ahead",
        "behind",
        "clean",
        "files_total",
        "files_returned",
        "files_truncated",
        "files_limit",
        "transport_safe",
        "output_truncated",
    ] {
        copy_scalar(output, &mut presentation, key);
    }
    if let Some(observation) = show_changes_status_observation(output) {
        presentation.insert("status_observation".to_string(), observation);
    }
    if let Some(short) = output.pointer("/head/short").and_then(short_git_commit) {
        presentation.insert("head".to_string(), json!({"short": short}));
    }
    if let Some(counts) = output.get("counts") {
        if counts.is_object() {
            let mut bounded_counts = Map::new();
            for key in [
                "modified",
                "added",
                "deleted",
                "renamed",
                "copied",
                "untracked",
                "conflicted",
                "staged",
                "unstaged",
            ] {
                copy_scalar(counts, &mut bounded_counts, key);
            }
            presentation.insert("counts".to_string(), Value::Object(bounded_counts));
        }
    }
    let (truncation_reasons, reasons_truncated) = output
        .get("truncation_reasons")
        .map(bounded_safe_labels)
        .unwrap_or_default();
    if !truncation_reasons.is_empty() {
        presentation.insert(
            "truncation_reasons".to_string(),
            Value::Array(truncation_reasons),
        );
    }
    if reasons_truncated {
        presentation.insert(
            "truncation_reasons_truncated".to_string(),
            Value::Bool(true),
        );
    }

    if let Some(source_files) = output.get("files").and_then(Value::as_array) {
        let clean = output.get("clean").and_then(Value::as_bool) == Some(true);
        let mut additions = 0u64;
        let mut deletions = 0u64;
        let mut line_stats_observed = false;
        let mut line_stats_partial = output
            .get("files_truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if clean {
            presentation.insert("additions".to_string(), Value::from(0));
            presentation.insert("deletions".to_string(), Value::from(0));
            presentation.insert("line_stats_partial".to_string(), Value::Bool(false));
        } else if !source_files.is_empty() {
            for file in source_files {
                match (
                    file.get("additions").and_then(Value::as_u64),
                    file.get("deletions").and_then(Value::as_u64),
                ) {
                    (Some(file_additions), Some(file_deletions)) => {
                        line_stats_observed = true;
                        additions = additions.saturating_add(file_additions);
                        deletions = deletions.saturating_add(file_deletions);
                    }
                    _ => line_stats_partial = true,
                }
            }
            if line_stats_observed {
                presentation.insert("additions".to_string(), Value::from(additions));
                presentation.insert("deletions".to_string(), Value::from(deletions));
                presentation.insert(
                    "line_stats_partial".to_string(),
                    Value::Bool(line_stats_partial),
                );
            }
        }
        let mut remaining_hunks = MAX_MCP_PRESENTATION_DIFF_HUNKS;
        let mut files = Vec::new();
        let mut items_truncated = false;
        for file in source_files {
            if files.len() == MAX_MCP_PRESENTATION_ITEMS {
                items_truncated = true;
                break;
            }
            if let Some(file) = show_changes_file_presentation(file, output, &mut remaining_hunks) {
                files.push(file);
            } else {
                items_truncated = true;
            }
        }
        let presentation_diff_truncated = files
            .iter()
            .any(|file| file.get("diff_truncated").and_then(Value::as_bool) == Some(true));
        if output.get("hunks_truncated").and_then(Value::as_bool) == Some(true)
            || presentation_diff_truncated
        {
            presentation.insert("diff_truncated".to_string(), Value::Bool(true));
        }
        presentation.insert("files".to_string(), Value::Array(files));
        presentation.insert(
            "items_truncated".to_string(),
            Value::Bool(items_truncated || source_files.len() > MAX_MCP_PRESENTATION_ITEMS),
        );
    }
    Some(Value::Object(presentation))
}

fn review_file_presentation(file: &Value) -> Option<Value> {
    file.as_object()?;
    let path_omitted = file
        .get("path_omitted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let path = if path_omitted {
        None
    } else {
        match file.get("path") {
            Some(value) if value.is_string() => Some(safe_repo_relative_path(value)?),
            _ => return None,
        }
    };
    let mut item = Map::new();
    if let Some(path) = path {
        item.insert("path".to_string(), Value::String(path));
    }
    item.insert("path_omitted".to_string(), Value::Bool(path_omitted));
    if let Some(previous_path) = file.get("previous_path").and_then(safe_repo_relative_path) {
        item.insert("previous_path".to_string(), Value::String(previous_path));
    }
    if let Some(status) = file.get("status").and_then(safe_label) {
        item.insert("status".to_string(), Value::String(status));
    }
    for key in ["additions", "deletions", "binary", "gitlink"] {
        copy_scalar(file, &mut item, key);
    }
    let (classes, classes_truncated) = file
        .get("classes")
        .map(bounded_safe_labels)
        .unwrap_or_default();
    if !classes.is_empty() {
        item.insert("classes".to_string(), Value::Array(classes));
    }
    if classes_truncated {
        item.insert("classes_truncated".to_string(), Value::Bool(true));
    }
    Some(Value::Object(item))
}

fn git_review_file_classes_presentation(output: &Value) -> Option<Value> {
    let classes = output.get("file_classes")?;
    classes.as_object()?;
    let mut result = Map::new();
    copy_scalar(classes, &mut result, "partial");
    if let Some(counts) = classes.get("counts_observed").and_then(Value::as_object) {
        let mut bounded_counts = Map::new();
        let mut truncated = false;
        for (key, value) in counts {
            if bounded_counts.len() == MAX_MCP_PRESENTATION_ITEMS {
                truncated = true;
                break;
            }
            let label = safe_label(&Value::String(key.clone()));
            if let Some(label) = label.filter(|_| value.is_number()) {
                bounded_counts.insert(label, value.clone());
            } else {
                truncated = true;
            }
        }
        result.insert("counts_observed".to_string(), Value::Object(bounded_counts));
        result.insert(
            "counts_truncated".to_string(),
            Value::Bool(truncated || counts.len() > MAX_MCP_PRESENTATION_ITEMS),
        );
    }
    Some(Value::Object(result))
}

fn git_review_presentation(output: &Value) -> Option<Value> {
    output.as_object()?;
    let mut presentation = Map::new();
    presentation.insert("version".to_string(), Value::from(MCP_PRESENTATION_VERSION));
    presentation.insert("kind".to_string(), Value::String("git_review".to_string()));
    for key in ["deterministic", "truncated"] {
        copy_scalar(output, &mut presentation, key);
    }
    if let Some(reason_code) = output.get("reason_code").and_then(safe_label) {
        presentation.insert("reason_code".to_string(), Value::String(reason_code));
    }
    if let Some(scope) = output.get("scope") {
        if scope.is_object() {
            let mut bounded_scope = Map::new();
            for (source, target) in [
                ("requested_base", "base"),
                ("requested_head", "head"),
                ("merge_base", "merge_base"),
            ] {
                if let Some(value) = scope.get(source).and_then(short_git_commit) {
                    bounded_scope.insert(target.to_string(), Value::String(value));
                }
            }
            for key in ["base_is_ancestor", "commit_count"] {
                copy_scalar(scope, &mut bounded_scope, key);
            }
            presentation.insert("scope".to_string(), Value::Object(bounded_scope));
        }
    }
    for (source_key, target_key, fields) in [
        (
            "stats",
            "stats",
            &["files_changed", "insertions", "deletions", "binary_files"][..],
        ),
        (
            "coverage",
            "coverage",
            &[
                "production_changed",
                "tests_changed",
                "docs_changed",
                "partial",
            ][..],
        ),
        (
            "truncation",
            "truncation",
            &[
                "files_total",
                "files_returned",
                "files_truncated",
                "classification_partial",
                "file_stats_partial",
                "file_modes_partial",
                "symbols_partial",
                "subsystems_partial",
                "signals_partial",
            ][..],
        ),
    ] {
        if let Some(source) = output.get(source_key).filter(|value| value.is_object()) {
            let mut target = Map::new();
            for key in fields {
                copy_scalar(source, &mut target, key);
            }
            presentation.insert(target_key.to_string(), Value::Object(target));
        }
    }
    if let Some(classes) = git_review_file_classes_presentation(output) {
        presentation.insert("file_classes".to_string(), classes);
    }
    if let Some(source_files) = output.get("files").and_then(Value::as_array) {
        let mut files = Vec::new();
        let mut items_truncated = false;
        for file in source_files {
            if files.len() == MAX_MCP_PRESENTATION_ITEMS {
                items_truncated = true;
                break;
            }
            if let Some(file) = review_file_presentation(file) {
                files.push(file);
            } else {
                items_truncated = true;
            }
        }
        presentation.insert("files".to_string(), Value::Array(files));
        presentation.insert(
            "items_truncated".to_string(),
            Value::Bool(items_truncated || source_files.len() > MAX_MCP_PRESENTATION_ITEMS),
        );
    }
    Some(Value::Object(presentation))
}

fn presentation_from_call_result(tool_name: &str, call_result: &Value) -> Option<Value> {
    if !tool_has_result_presentation_projection(tool_name) {
        return None;
    }
    let structured = call_result.get("structuredContent")?;
    let output = structured.get("output")?;
    match tool_name {
        "list_jobs" => list_jobs_presentation(output),
        "observe_jobs" => observe_jobs_presentation(output),
        "cargo_check" | "cargo_test" | "go_test" => validation_run_presentation(tool_name, output),
        "validation_summary" => validation_summary_presentation(output),
        "show_changes" => show_changes_presentation(output),
        "git_review_summary" => git_review_presentation(output),
        _ => None,
    }
}

pub(super) fn attach_result_app_presentation(tool_name: &str, call_result: &mut Value) {
    let Some(presentation) = presentation_from_call_result(tool_name, call_result) else {
        return;
    };
    let Some(result) = call_result.as_object_mut() else {
        return;
    };
    let meta = result
        .entry("_meta".to_string())
        .or_insert_with(|| json!({}));
    let Some(meta) = meta.as_object_mut() else {
        return;
    };
    meta.insert(MCP_PRESENTATION_META_KEY.to_string(), presentation);
}
