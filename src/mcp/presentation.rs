use serde_json::{json, Map, Value};

pub(super) const MCP_PRESENTATION_META_KEY: &str = "webcodex/presentation";
pub(super) const MCP_PRESENTATION_VERSION: u64 = 1;
pub(super) const MAX_MCP_PRESENTATION_ITEMS: usize = 8;
pub(super) const MAX_MCP_PRESENTATION_TEXT_CHARS: usize = 256;

/// Static MCP App descriptor/presentation eligibility only. This is not a Tool
/// registry or authority surface: the normal ToolSpec/runtime admission path
/// remains canonical and decides whether any of these tools are callable.
pub(super) fn tool_supports_result_app(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "list_jobs"
            | "observe_jobs"
            | "cargo_check"
            | "cargo_test"
            | "go_test"
            | "validation_summary"
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
        Some("reconciled") => {
            Some("Job state was reconciled from the canonical runtime record.")
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
    for key in [
        "active",
        "blocking_active",
        "terminal",
        "terminal_pending",
        "duration_ms",
        "elapsed_secs",
        "exit_code",
    ] {
        copy_scalar(job, &mut item, key);
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
    let items = jobs
        .iter()
        .take(MAX_MCP_PRESENTATION_ITEMS)
        .filter_map(job_summary_presentation)
        .collect::<Vec<_>>();
    Some(json!({
        "version": MCP_PRESENTATION_VERSION,
        "kind": "job_list",
        "count": output.get("count").and_then(Value::as_u64)?,
        "matched_count": output.get("matched_count").and_then(Value::as_u64)?,
        "truncated": output.get("truncated").and_then(Value::as_bool)?,
        "items_truncated": jobs.len() > MAX_MCP_PRESENTATION_ITEMS,
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
    item.insert("status".to_string(), Value::String(status));
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
        "terminal",
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

fn presentation_from_call_result(tool_name: &str, call_result: &Value) -> Option<Value> {
    if !tool_supports_result_app(tool_name) {
        return None;
    }
    let structured = call_result.get("structuredContent")?;
    let output = structured.get("output")?;
    match tool_name {
        "list_jobs" => list_jobs_presentation(output),
        "observe_jobs" => observe_jobs_presentation(output),
        "cargo_check" | "cargo_test" | "go_test" => validation_run_presentation(tool_name, output),
        "validation_summary" => validation_summary_presentation(output),
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
