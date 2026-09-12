//! Bounded multi-Job observation composed from the canonical single-Job path.

use super::{ObserveJobsItem, RecoveryKind, RecoveryTool, ToolResult, ToolRuntime};
use crate::auth::AuthContext;
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Duration;
use tokio::time::Instant;
use webcodex_core::runtime_contract::MODEL_INSPECTION_MAX_RESULT_BYTES;
use webcodex_workspace::file_read_normalize::MODEL_RESULT_ENVELOPE_RESERVE_BYTES;

pub(crate) const MAX_OBSERVE_JOBS_ITEMS: usize = 8;
pub(crate) const MAX_OBSERVE_JOBS_TAIL_LINES: usize = 200;
const MAX_OBSERVE_JOBS_WAIT_SECS: u64 = 60;
/// Final serialized model-facing budget for packing multiple already-bounded
/// Job observations. This does not change any single Job stream/tail retention.
const MAX_OBSERVE_JOBS_AGGREGATE_RESULT_BYTES: usize = MODEL_INSPECTION_MAX_RESULT_BYTES;
const MAX_OBSERVE_JOBS_ERROR_CHARS: usize = 512;

#[derive(Debug)]
struct ObservedJob {
    index: usize,
    job_id: String,
    result: ToolResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WakeReason {
    Immediate,
    Updated,
    Terminal,
    ItemError,
    Timeout,
}

impl WakeReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::Updated => "updated",
            Self::Terminal => "terminal",
            Self::ItemError => "item_error",
            Self::Timeout => "timeout",
        }
    }
}

fn observed_has_error(observed: &[ObservedJob]) -> bool {
    observed.iter().any(|item| !item.result.success)
}

fn observed_has_terminal(observed: &[ObservedJob]) -> bool {
    observed
        .iter()
        .any(|item| item.result.output["terminal"].as_bool() == Some(true))
}

fn observed_has_change(observed: &[ObservedJob]) -> bool {
    observed
        .iter()
        .any(|item| item.result.output["changed"].as_bool() == Some(true))
}

fn bounded_error(error: Option<&str>) -> String {
    let error = error.unwrap_or("Job observation failed");
    let mut chars = error.chars();
    let bounded = chars
        .by_ref()
        .take(MAX_OBSERVE_JOBS_ERROR_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        format!("{bounded}…")
    } else {
        bounded
    }
}

fn observation_error_kind(result: &ToolResult) -> &'static str {
    let error = result.error.as_deref().unwrap_or_default();
    if error.contains("after_observation_token") {
        "invalid_observation_token"
    } else if error.starts_with("unknown job:") {
        "unknown_job"
    } else if error.contains("local_job_observation") {
        "observation_failed"
    } else {
        "job_observation_failed"
    }
}

fn observation_recovery(error_kind: &str) -> (RecoveryKind, Option<RecoveryTool>) {
    match error_kind {
        "invalid_observation_token" | "output_budget_exceeded" => (RecoveryKind::FixInput, None),
        "unknown_job" => (RecoveryKind::Reobserve, Some(RecoveryTool::ListJobs)),
        _ => (RecoveryKind::NoAction, None),
    }
}

fn batch_item(observed: ObservedJob) -> Value {
    if observed.result.success {
        let mut output = observed.result.output;
        if let Some(output) = output.as_object_mut() {
            // observe_jobs owns one shared wait for the batch. The final item
            // refreshes are snapshots only; leaking job_log's internal
            // non-waiting metadata would present a second, contradictory wait
            // fact to the model.
            output.remove("wait_outcome");
            output.remove("waited_ms");
        }
        json!({
            "index": observed.index,
            "job_id": observed.job_id,
            "success": true,
            "output": output,
            "error_kind": null,
            "error": null,
        })
    } else {
        let error_kind = observation_error_kind(&observed.result);
        let (recovery_kind, recovery_tool) = observation_recovery(error_kind);
        let mut item = json!({
            "index": observed.index,
            "job_id": observed.job_id,
            "success": false,
            "output": null,
            "error_kind": error_kind,
            "recovery_kind": recovery_kind.as_str(),
            "error": bounded_error(observed.result.error.as_deref()),
        });
        if let Some(recovery_tool) = recovery_tool {
            item["recovery_tool"] = json!(recovery_tool.as_str());
        }
        item
    }
}

fn output_budget_failure_item(index: usize, job_id: String) -> Value {
    json!({
        "index": index,
        "job_id": job_id,
        "success": false,
        "output": null,
        "error_kind": "output_budget_exceeded",
        "recovery_kind": RecoveryKind::FixInput.as_str(),
        "error": "The bounded Job observation cannot fit in one model result; resubmit this Job with a smaller tail_lines value.",
    })
}

fn batch_output(
    requested_count: usize,
    items: Vec<Value>,
    wake_reason: WakeReason,
    waited_ms: u64,
    output_truncated: bool,
    next_index: Option<usize>,
) -> Value {
    let succeeded_count = items
        .iter()
        .filter(|item| item["success"].as_bool() == Some(true))
        .count();
    let returned_count = items.len();
    let changed_count = items
        .iter()
        .filter(|item| {
            item["success"].as_bool() == Some(true)
                && item["output"]["changed"].as_bool() == Some(true)
        })
        .count();
    let terminal_count = items
        .iter()
        .filter(|item| {
            item["success"].as_bool() == Some(true)
                && item["output"]["terminal"].as_bool() == Some(true)
        })
        .count();
    json!({
        "requested_count": requested_count,
        "returned_count": returned_count,
        "succeeded_count": succeeded_count,
        "failed_count": returned_count - succeeded_count,
        "items": items,
        "wait": {
            "outcome": wake_reason.as_str(),
            "waited_ms": waited_ms,
        },
        "changed_count": changed_count,
        "terminal_count": terminal_count,
        "output_truncated": output_truncated,
        "next_index": next_index,
    })
}

fn serialized_batch_fits(output: &Value) -> bool {
    serde_json::to_vec(&ToolResult::ok(output.clone()))
        .map(|bytes| {
            bytes.len()
                <= MAX_OBSERVE_JOBS_AGGREGATE_RESULT_BYTES
                    .saturating_sub(MODEL_RESULT_ENVELOPE_RESERVE_BYTES)
        })
        .unwrap_or(false)
}

fn apply_output_budget(
    requested_count: usize,
    completed: Vec<Value>,
    wake_reason: WakeReason,
    waited_ms: u64,
) -> Result<Value, String> {
    let mut returned = Vec::with_capacity(completed.len());
    let mut next_index = None;

    for item in completed {
        let index = item["index"].as_u64().unwrap_or(returned.len() as u64) as usize;
        let mut candidate_items = returned.clone();
        candidate_items.push(item.clone());
        let candidate = batch_output(
            requested_count,
            candidate_items,
            wake_reason,
            waited_ms,
            false,
            None,
        );
        if serialized_batch_fits(&candidate) {
            returned.push(item);
            continue;
        }

        let single = batch_output(
            requested_count,
            vec![item.clone()],
            wake_reason,
            waited_ms,
            false,
            None,
        );
        if serialized_batch_fits(&single) {
            next_index = Some(index);
            break;
        }

        let budget_failure =
            output_budget_failure_item(index, item["job_id"].as_str().unwrap_or_default().into());
        let mut candidate_items = returned.clone();
        candidate_items.push(budget_failure.clone());
        let candidate = batch_output(
            requested_count,
            candidate_items,
            wake_reason,
            waited_ms,
            false,
            None,
        );
        if !serialized_batch_fits(&candidate) {
            if returned.is_empty() {
                return Err(
                    "observe_jobs could not encode a bounded output-budget failure item".into(),
                );
            }
            next_index = Some(index);
            break;
        }
        returned.push(budget_failure);
    }

    Ok(batch_output(
        requested_count,
        returned,
        wake_reason,
        waited_ms,
        next_index.is_some(),
        next_index,
    ))
}

fn copy_non_null(
    source: &serde_json::Map<String, Value>,
    target: &mut serde_json::Map<String, Value>,
    key: &str,
) {
    if let Some(value) = source.get(key).filter(|value| !value.is_null()) {
        target.insert(key.to_string(), value.clone());
    }
}

fn copy_present(
    source: &serde_json::Map<String, Value>,
    target: &mut serde_json::Map<String, Value>,
    key: &str,
) {
    if let Some(value) = source.get(key) {
        target.insert(key.to_string(), value.clone());
    }
}

fn sparse_success_item(item: &Value) -> Option<Value> {
    let item = item.as_object()?;
    if item.get("success").and_then(Value::as_bool) != Some(true)
        || !item.get("error_kind").is_some_and(Value::is_null)
        || !item.get("error").is_some_and(Value::is_null)
        || item.get("recovery_kind").is_some()
        || item.get("recovery_tool").is_some()
    {
        return None;
    }
    let job_id = item.get("job_id")?.as_str()?.to_string();
    let observation = item.get("output")?.as_object()?;
    if observation.get("job_id")?.as_str()? != job_id {
        return None;
    }
    let status = observation.get("status")?.as_str()?.to_string();
    let terminal = observation.get("terminal")?.as_bool()?;
    let changed = observation.get("changed")?.as_bool()?;
    let log_delta_status = observation.get("log_delta_status")?.as_str()?;
    if !matches!(
        log_delta_status,
        "baseline" | "delta" | "unchanged" | "reset"
    ) {
        return None;
    }
    let observation_token = observation.get("observation_token")?.as_str()?;
    if observation_token.is_empty() {
        return None;
    }
    let stdout_tail = observation.get("stdout_tail")?.as_str()?;
    let stderr_tail = observation.get("stderr_tail")?.as_str()?;
    let stdout_truncated = observation.get("stdout_truncated")?.as_bool()?;
    let stderr_truncated = observation.get("stderr_truncated")?.as_bool()?;
    let stdout_delta_reset = observation.get("stdout_delta_reset")?.as_bool()?;
    let stderr_delta_reset = observation.get("stderr_delta_reset")?.as_bool()?;
    let earlier_stdout_unavailable = observation
        .get("earlier_stdout_unavailable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let earlier_stderr_unavailable = observation
        .get("earlier_stderr_unavailable")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut sparse = serde_json::Map::new();
    sparse.insert("job_id".to_string(), json!(job_id));
    sparse.insert("status".to_string(), json!(status));
    sparse.insert("terminal".to_string(), json!(terminal));
    sparse.insert("changed".to_string(), json!(changed));
    sparse.insert("log_delta_status".to_string(), json!(log_delta_status));
    sparse.insert("observation_token".to_string(), json!(observation_token));

    for key in [
        "exit_code",
        "command_execution_state",
        "activity",
        "detected_summary",
        "validation",
        "ssh_resource",
    ] {
        copy_non_null(observation, &mut sparse, key);
    }

    if log_delta_status != "unchanged" {
        copy_present(observation, &mut sparse, "stdout_tail");
        copy_present(observation, &mut sparse, "stderr_tail");
    } else {
        if !stdout_tail.is_empty() {
            copy_present(observation, &mut sparse, "stdout_tail");
        }
        if !stderr_tail.is_empty() {
            copy_present(observation, &mut sparse, "stderr_tail");
        }
    }

    for (key, present) in [
        ("stdout_truncated", stdout_truncated),
        ("stderr_truncated", stderr_truncated),
        ("stdout_delta_reset", stdout_delta_reset),
        ("stderr_delta_reset", stderr_delta_reset),
        ("earlier_stdout_unavailable", earlier_stdout_unavailable),
        ("earlier_stderr_unavailable", earlier_stderr_unavailable),
    ] {
        if present {
            copy_present(observation, &mut sparse, key);
        }
    }
    for key in ["recovery_state", "recovery_reason_code", "recovery_reason"] {
        copy_non_null(observation, &mut sparse, key);
    }

    let exceptional_log_evidence = log_delta_status == "reset"
        || stdout_truncated
        || stderr_truncated
        || stdout_delta_reset
        || stderr_delta_reset
        || earlier_stdout_unavailable
        || earlier_stderr_unavailable
        || observation
            .get("recovery_state")
            .is_some_and(|value| !value.is_null())
        || observation
            .get("recovery_reason_code")
            .is_some_and(|value| !value.is_null())
        || observation
            .get("recovery_reason")
            .is_some_and(|value| !value.is_null());
    if exceptional_log_evidence {
        for key in [
            "stdout_lines",
            "stderr_lines",
            "stdout_returned_lines",
            "stderr_returned_lines",
            "stdout_retained_from_line",
            "stderr_retained_from_line",
            "cursor",
            "last_update_seq",
        ] {
            copy_non_null(observation, &mut sparse, key);
        }
        if log_delta_status == "reset" {
            for key in [
                "stdout_truncated",
                "stderr_truncated",
                "stdout_delta_reset",
                "stderr_delta_reset",
                "earlier_stdout_unavailable",
                "earlier_stderr_unavailable",
            ] {
                copy_present(observation, &mut sparse, key);
            }
        }
    }

    if matches!(log_delta_status, "baseline" | "reset") {
        for key in ["purpose", "command_summary"] {
            copy_non_null(observation, &mut sparse, key);
        }
    }
    Some(Value::Object(sparse))
}

/// Final model-facing projection for ordinary successful Job observations.
/// The canonical batch and canonical single-Job snapshots remain unchanged for
/// budgeting, Session/audit recording, operator diagnostics, and internal use.
pub(crate) fn sparsify_observe_jobs_model_result(result: &mut ToolResult) {
    if !result.success {
        return;
    }
    let Some(output) = result.output.as_object_mut() else {
        return;
    };
    if output.get("output_truncated").and_then(Value::as_bool) != Some(false)
        || !output.get("next_index").is_some_and(Value::is_null)
    {
        return;
    }
    let Some(items) = output.get("items").and_then(Value::as_array) else {
        return;
    };
    if items.is_empty() || items.len() > MAX_OBSERVE_JOBS_ITEMS {
        return;
    }
    let count = items.len() as u64;
    if output.get("requested_count").and_then(Value::as_u64) != Some(count)
        || output.get("returned_count").and_then(Value::as_u64) != Some(count)
        || output.get("succeeded_count").and_then(Value::as_u64) != Some(count)
        || output.get("failed_count").and_then(Value::as_u64) != Some(0)
    {
        return;
    }
    let Some(wait) = output.get("wait").and_then(Value::as_object) else {
        return;
    };
    let Some(wait_outcome) = wait.get("outcome").and_then(Value::as_str) else {
        return;
    };
    if !matches!(
        wait_outcome,
        "immediate" | "updated" | "terminal" | "timeout"
    ) {
        // item_error is intentionally kept in the canonical batch shape.
        return;
    }
    let Some(waited_ms) = wait.get("waited_ms").and_then(Value::as_u64) else {
        return;
    };

    let mut sparse_items = Vec::with_capacity(items.len());
    let mut changed_count = 0u64;
    let mut terminal_count = 0u64;
    for item in items {
        let Some(sparse) = sparse_success_item(item) else {
            return;
        };
        changed_count += u64::from(sparse.get("changed").and_then(Value::as_bool) == Some(true));
        terminal_count += u64::from(sparse.get("terminal").and_then(Value::as_bool) == Some(true));
        sparse_items.push(sparse);
    }
    if output.get("changed_count").and_then(Value::as_u64) != Some(changed_count)
        || output.get("terminal_count").and_then(Value::as_u64) != Some(terminal_count)
    {
        return;
    }

    output.insert("items".to_string(), Value::Array(sparse_items));
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
        output.remove(key);
    }
    if waited_ms == 0 {
        if let Some(wait) = output.get_mut("wait").and_then(Value::as_object_mut) {
            wait.remove("waited_ms");
        }
    }
}

fn normalize_observe_jobs_preferences(
    tail_lines: usize,
    wait_secs: Option<u64>,
) -> (usize, Option<u64>) {
    (
        tail_lines.min(MAX_OBSERVE_JOBS_TAIL_LINES),
        wait_secs.map(|wait_secs| wait_secs.min(MAX_OBSERVE_JOBS_WAIT_SECS)),
    )
}

impl ToolRuntime {
    fn validate_observe_jobs_input(
        items: &[ObserveJobsItem],
        tail_lines: usize,
        wait_secs: Option<u64>,
    ) -> Result<(), String> {
        if !(1..=MAX_OBSERVE_JOBS_ITEMS).contains(&items.len()) {
            return Err("observe_jobs requires between 1 and 8 items".into());
        }
        if items.iter().any(|item| item.job_id.trim().is_empty()) {
            return Err("observe_jobs requires every item to have a non-empty job_id".into());
        }
        if let Some(item) = items.iter().find(|item| {
            item.after_observation_token.as_ref().is_some_and(|token| {
                token.len() > crate::job_observation::MAX_JOB_OBSERVATION_TOKEN_LEN
            })
        }) {
            return Err(format!(
                "observe_jobs token for job_id {} exceeds 192 bytes",
                item.job_id
            ));
        }
        if tail_lines == 0 {
            return Err("observe_jobs tail_lines must be at least 1".into());
        }
        if wait_secs == Some(0) {
            return Err("observe_jobs wait_secs must be at least 1".into());
        }
        let mut seen = HashSet::with_capacity(items.len());
        if let Some(duplicate) = items
            .iter()
            .map(|item| item.job_id.as_str())
            .find(|job_id| !seen.insert(*job_id))
        {
            return Err(format!(
                "observe_jobs rejects duplicate job_id values: {duplicate}"
            ));
        }
        Ok(())
    }

    async fn observe_jobs_pass(
        &self,
        items: &[ObserveJobsItem],
        tail_lines: usize,
        auth: Option<&AuthContext>,
    ) -> Vec<ObservedJob> {
        let mut observed: Vec<ObservedJob> = stream::iter(items.iter().cloned().enumerate().map(
            |(index, item)| async move {
                let result = self
                    .job_log_for_auth(
                        item.job_id.clone(),
                        None,
                        Some(tail_lines),
                        auth,
                        item.after_observation_token,
                        None,
                    )
                    .await;
                ObservedJob {
                    index,
                    job_id: item.job_id,
                    result,
                }
            },
        ))
        .buffer_unordered(MAX_OBSERVE_JOBS_ITEMS)
        .collect()
        .await;
        observed.sort_by_key(|item| item.index);
        observed
    }

    async fn wait_for_any_observed_job(
        &self,
        items: &[ObserveJobsItem],
        auth: Option<&AuthContext>,
        wait_secs: u64,
    ) -> Result<WakeReason, String> {
        let deadline = Instant::now() + Duration::from_secs(wait_secs);
        loop {
            let mut waits = stream::iter(items.iter().cloned().enumerate().map(
                |(index, item)| async move {
                    let result = self
                        .job_log_for_auth(
                            item.job_id.clone(),
                            None,
                            Some(1),
                            auth,
                            item.after_observation_token,
                            Some(wait_secs),
                        )
                        .await;
                    ObservedJob {
                        index,
                        job_id: item.job_id,
                        result,
                    }
                },
            ))
            .buffer_unordered(MAX_OBSERVE_JOBS_ITEMS);
            let heartbeat = (Instant::now() + Duration::from_millis(200)).min(deadline);
            tokio::select! {
                first = waits.next() => {
                    let first = first.ok_or_else(|| {
                        "observe_jobs shared wait had no item futures".to_string()
                    })?;
                    if !first.result.success {
                        return Ok(WakeReason::ItemError);
                    }
                    if first.result.output["terminal"].as_bool() == Some(true) {
                        return Ok(WakeReason::Terminal);
                    }
                    if first.result.output["changed"].as_bool() == Some(true) {
                        return Ok(WakeReason::Updated);
                    }
                    match first.result.output["wait_outcome"].as_str() {
                        Some("terminal") => return Ok(WakeReason::Terminal),
                        Some("updated" | "immediate") => return Ok(WakeReason::Updated),
                        Some("timeout") if Instant::now() >= deadline => {
                            return Ok(WakeReason::Timeout);
                        }
                        Some("timeout") => {}
                        _ => {
                            return Err(
                                "observe_jobs canonical wait returned an invalid wait outcome"
                                    .into(),
                            );
                        }
                    }
                }
                _ = tokio::time::sleep_until(heartbeat) => {}
            }
            drop(waits);

            // Agent notifications are an optimization, not a second source of
            // truth. Re-enter the canonical immediate path on one shared
            // heartbeat so a notification race cannot defer a visible token
            // change until the full deadline. These one-line snapshots are
            // discarded; the caller performs the final requested-tail refresh.
            let heartbeat_observation = self.observe_jobs_pass(items, 1, auth).await;
            if observed_has_error(&heartbeat_observation) {
                return Ok(WakeReason::ItemError);
            }
            if observed_has_terminal(&heartbeat_observation) {
                return Ok(WakeReason::Terminal);
            }
            if observed_has_change(&heartbeat_observation) {
                return Ok(WakeReason::Updated);
            }
            if Instant::now() >= deadline {
                return Ok(WakeReason::Timeout);
            }
        }
    }

    pub(crate) async fn observe_jobs_for_auth(
        &self,
        items: Vec<ObserveJobsItem>,
        tail_lines: usize,
        wait_secs: Option<u64>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let (tail_lines, wait_secs) = normalize_observe_jobs_preferences(tail_lines, wait_secs);
        if let Err(error) = Self::validate_observe_jobs_input(&items, tail_lines, wait_secs) {
            return ToolResult::err(error);
        }

        let requested_count = items.len();
        let initial = self.observe_jobs_pass(&items, tail_lines, auth).await;
        let missing_baseline = items
            .iter()
            .any(|item| item.after_observation_token.is_none());
        let immediate_reason = if wait_secs.is_none() || missing_baseline {
            Some(WakeReason::Immediate)
        } else if observed_has_error(&initial) {
            Some(WakeReason::ItemError)
        } else if observed_has_terminal(&initial) {
            Some(WakeReason::Terminal)
        } else if observed_has_change(&initial) {
            Some(WakeReason::Updated)
        } else {
            None
        };

        let (observed, wake_reason, waited_ms) = if let Some(reason) = immediate_reason {
            (initial, reason, 0)
        } else {
            let wait_secs = wait_secs.expect("shared wait requires validated wait_secs");
            let wait_started = Instant::now();
            let wait_reason = match self
                .wait_for_any_observed_job(&items, auth, wait_secs)
                .await
            {
                Ok(reason) => reason,
                Err(error) => return ToolResult::err(error),
            };
            let waited_ms = wait_started.elapsed().as_millis() as u64;
            let refreshed = self.observe_jobs_pass(&items, tail_lines, auth).await;
            let final_reason =
                if observed_has_error(&refreshed) || wait_reason == WakeReason::ItemError {
                    WakeReason::ItemError
                } else if observed_has_terminal(&refreshed) || wait_reason == WakeReason::Terminal {
                    WakeReason::Terminal
                } else if observed_has_change(&refreshed) || wait_reason == WakeReason::Updated {
                    WakeReason::Updated
                } else {
                    WakeReason::Timeout
                };
            (refreshed, final_reason, waited_ms)
        };

        let completed = observed.into_iter().map(batch_item).collect();
        match apply_output_budget(requested_count, completed, wake_reason, waited_ms) {
            Ok(output) => ToolResult::ok(output),
            Err(error) => ToolResult::err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_error_limits_character_count() {
        let error = "雪".repeat(MAX_OBSERVE_JOBS_ERROR_CHARS + 10);
        let bounded = bounded_error(Some(&error));
        assert_eq!(bounded.chars().count(), MAX_OBSERVE_JOBS_ERROR_CHARS + 1);
        assert!(bounded.ends_with('…'));
    }

    #[test]
    fn oversized_observation_preferences_are_clamped() {
        assert_eq!(
            normalize_observe_jobs_preferences(500, Some(120)),
            (
                MAX_OBSERVE_JOBS_TAIL_LINES,
                Some(MAX_OBSERVE_JOBS_WAIT_SECS)
            )
        );
        assert_eq!(
            normalize_observe_jobs_preferences(40, Some(5)),
            (40, Some(5))
        );
    }

    #[test]
    fn batch_item_failures_expose_bounded_recovery_and_success_omits_it() {
        let missing = batch_item(ObservedJob {
            index: 0,
            job_id: "job-missing".to_string(),
            result: ToolResult::err("unknown job: job-missing"),
        });
        assert_eq!(missing["error_kind"], "unknown_job");
        assert_eq!(missing["recovery_kind"], "reobserve");
        assert_eq!(missing["recovery_tool"], "list_jobs");
        assert!(
            crate::tool_runtime::tool_definition::is_adaptive_runtime_direct_tool(
                missing["recovery_tool"].as_str().unwrap()
            )
        );

        let invalid_token = batch_item(ObservedJob {
            index: 1,
            job_id: "job-token".to_string(),
            result: ToolResult::err("invalid after_observation_token"),
        });
        assert_eq!(invalid_token["recovery_kind"], "fix_input");
        assert!(invalid_token.get("recovery_tool").is_none());

        let success = batch_item(ObservedJob {
            index: 2,
            job_id: "job-ok".to_string(),
            result: ToolResult::ok(json!({"changed": false})),
        });
        assert!(success.get("recovery_kind").is_none());
        assert!(success.get("recovery_tool").is_none());
    }

    #[test]
    fn output_budget_replaces_one_oversized_item_without_partial_json() {
        let item = json!({
            "index": 0,
            "job_id": "job-0",
            "success": true,
            "output": {
                "changed": false,
                "terminal": false,
                "stdout_tail": "x".repeat(MAX_OBSERVE_JOBS_AGGREGATE_RESULT_BYTES),
            },
            "error_kind": null,
            "error": null,
        });
        let output = apply_output_budget(1, vec![item], WakeReason::Immediate, 0).unwrap();
        assert_eq!(output["wait"]["outcome"], "immediate");
        assert_eq!(output["wait"]["waited_ms"], 0);
        assert_eq!(output["returned_count"], 1);
        assert_eq!(output["items"][0]["success"], false);
        assert_eq!(output["items"][0]["error_kind"], "output_budget_exceeded");
        assert_eq!(output["items"][0]["recovery_kind"], "fix_input");
        assert_eq!(output["output_truncated"], false);
        assert!(
            serde_json::to_vec(&ToolResult::ok(output)).unwrap().len()
                <= MAX_OBSERVE_JOBS_AGGREGATE_RESULT_BYTES
        );
    }

    #[test]
    fn output_budget_keeps_whole_prefix_and_points_at_first_omitted_index() {
        let item = |index| {
            json!({
                "index": index,
                "job_id": format!("job-{index}"),
                "success": true,
                "output": {
                    "changed": false,
                    "terminal": false,
                    "stdout_tail": "x".repeat(90_000),
                },
                "error_kind": null,
                "error": null,
            })
        };
        let output = apply_output_budget(
            4,
            vec![item(0), item(1), item(2), item(3)],
            WakeReason::Immediate,
            0,
        )
        .unwrap();
        // Four ~90 KiB observations straddle the old 256 KiB aggregate budget
        // but fit comfortably inside the explicit 512 KiB model-facing packer.
        assert_eq!(output["returned_count"], 4);
        assert_eq!(output["output_truncated"], false);
        assert!(output["next_index"].is_null());
        assert_eq!(output["wait"]["outcome"], "immediate");
        assert!(
            serde_json::to_vec(&ToolResult::ok(output)).unwrap().len()
                <= MAX_OBSERVE_JOBS_AGGREGATE_RESULT_BYTES
        );

        let output =
            apply_output_budget(8, (0..8).map(item).collect(), WakeReason::Immediate, 0).unwrap();
        assert!(output["returned_count"].as_u64().unwrap() > 2);
        assert!(output["returned_count"].as_u64().unwrap() < 8);
        assert_eq!(output["output_truncated"], true);
        assert_eq!(
            output["next_index"], output["returned_count"],
            "next_index must identify the first whole observation omitted by aggregate packing"
        );
        assert!(
            serde_json::to_vec(&ToolResult::ok(output)).unwrap().len()
                <= MAX_OBSERVE_JOBS_AGGREGATE_RESULT_BYTES
        );
    }

    #[test]
    fn aggregate_ceiling_does_not_expand_single_job_snapshot_or_tail_contracts() {
        assert_eq!(MAX_OBSERVE_JOBS_AGGREGATE_RESULT_BYTES, 512 * 1024);
        assert_eq!(MAX_OBSERVE_JOBS_TAIL_LINES, 200);
        assert_eq!(
            webcodex_core::runtime_contract::DEFAULT_OBSERVE_JOBS_TAIL_LINES,
            40
        );
        assert_eq!(
            webcodex_core::runner_protocol::JOB_SNAPSHOT_STREAM_MAX_BYTES,
            64 * 1024
        );
    }
}
