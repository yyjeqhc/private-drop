use serde_json::{json, Map, Value};
use webcodex_core::runner_job_lifecycle::RunnerJobLifecycle;

pub(super) const MCP_PRESENTATION_META_KEY: &str = "webcodex/presentation";
pub(super) const MCP_PRESENTATION_VERSION: u64 = 1;
pub(super) const MAX_MCP_PRESENTATION_ITEMS: usize = 8;
pub(super) const MAX_MCP_PRESENTATION_TEXT_CHARS: usize = 256;

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
    copy_scalar(job, &mut item, "duration_ms");
    copy_scalar(job, &mut item, "elapsed_secs");
    copy_scalar(job, &mut item, "exit_code");
    if let Ok(lifecycle) = RunnerJobLifecycle::from_wire(&status) {
        item.insert("terminal".to_string(), Value::Bool(lifecycle.is_terminal()));
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

fn presentation_from_call_result(tool_name: &str, call_result: &Value) -> Option<Value> {
    let structured = call_result.get("structuredContent")?;
    let output = structured.get("output")?;
    match tool_name {
        "list_jobs" => list_jobs_presentation(output),
        "observe_jobs" => observe_jobs_presentation(output),
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
