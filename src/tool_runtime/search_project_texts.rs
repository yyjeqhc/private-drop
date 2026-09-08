//! Bounded independent text searches built from the canonical single-search core.

use super::files::{SearchOptions, SearchRequest};
use super::project_resolution::ResolvedProject;
use super::{SearchProjectTextsQuery, ToolResult, ToolRuntime};
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::Instant;
use webcodex_workspace::file_read_normalize::MODEL_RESULT_ENVELOPE_RESERVE_BYTES;
use webcodex_workspace::file_read_range::MAX_SERIALIZED_OUTPUT_BYTES;

pub(crate) const MAX_SEARCH_PROJECT_TEXTS_QUERIES: usize = 8;
// Keep search fanout below the query cap: each rg process can independently
// consume CPU, filesystem bandwidth, and page cache, including while a heavy
// validation Job is running. Two is the cross-device baseline after current
// 2-vs-4 narrow/broad workload review; results still preserve input order.
pub(crate) const MAX_SEARCH_PROJECT_TEXTS_CONCURRENCY: usize = 2;
pub(crate) const DEFAULT_SEARCH_PROJECT_TEXTS_DEADLINE: Duration = Duration::from_secs(30);
pub(crate) use webcodex_core::runtime_contract::{
    DEFAULT_SEARCH_PROJECT_TEXTS_RESULT_BYTES, MIN_SEARCH_PROJECT_TEXTS_RESULT_BYTES,
};

fn search_request_and_pattern_mode(
    query: SearchProjectTextsQuery,
) -> (SearchRequest, Option<super::SearchPatternMode>) {
    let SearchProjectTextsQuery {
        pattern,
        pattern_mode,
        path,
        limit,
        context_before,
        context_after,
        include_globs,
        exclude_globs,
        result_mode,
        timeout_secs,
    } = query;
    (
        SearchRequest {
            pattern,
            path,
            limit,
            context_before,
            context_after,
            include_globs,
            exclude_globs,
            result_mode,
            timeout_secs,
        },
        pattern_mode,
    )
}

fn normalized_result_budget(max_result_bytes: Option<usize>) -> usize {
    max_result_bytes
        .unwrap_or(DEFAULT_SEARCH_PROJECT_TEXTS_RESULT_BYTES)
        .clamp(
            MIN_SEARCH_PROJECT_TEXTS_RESULT_BYTES,
            MAX_SERIALIZED_OUTPUT_BYTES,
        )
}

fn batch_output(
    project: &str,
    requested_count: usize,
    items: Vec<Value>,
    output_truncated: bool,
    next_index: Option<usize>,
    truncation_reason: Option<&str>,
) -> Value {
    let succeeded_count = items
        .iter()
        .filter(|item| item["success"].as_bool() == Some(true))
        .count();
    let returned_count = items.len();
    let mut output = json!({
        "project": project,
        "requested_count": requested_count,
        "returned_count": returned_count,
        "succeeded_count": succeeded_count,
        "failed_count": returned_count - succeeded_count,
        "items": items,
        "output_truncated": output_truncated,
        "next_index": next_index,
    });
    if let Some(reason) = truncation_reason {
        output["truncation_reason"] = json!(reason);
    }
    output
}

#[cfg(test)]
fn serialized_batch_len(output: &Value) -> usize {
    serde_json::to_vec(&ToolResult::ok(output.clone()))
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

fn serialized_value_len(value: &Value) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

fn projected_batch_serialized_len(output: &Value, default_timeouts: &[bool]) -> usize {
    let mut projected = ToolResult::ok(output.clone());
    super::dispatch::sparsify_search_batch_success_for_model(default_timeouts, &mut projected);
    serde_json::to_vec(&projected)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

fn projected_search_item_len(item: &Value, default_timeout: bool) -> usize {
    let mut projected = item.clone();
    if projected["success"].as_bool() == Some(true) {
        if let Some(output) = projected.get_mut("output").and_then(Value::as_object_mut) {
            super::dispatch::sparsify_search_output_for_model(output, default_timeout, true);
        }
    }
    serialized_value_len(&projected)
}

fn projected_batch_len(base_len: usize, item_bytes: usize, item_count: usize) -> usize {
    base_len
        .saturating_add(item_bytes)
        .saturating_add(item_count.saturating_sub(1))
}

fn retryable_runner_request_failure(result: &ToolResult) -> bool {
    !result.success
        && result.output.get("code").and_then(Value::as_str) == Some("search_request_dropped")
}

fn apply_output_budget(
    project: &str,
    requested_count: usize,
    completed: Vec<Value>,
    default_timeouts: &[bool],
    max_result_bytes: Option<usize>,
) -> Value {
    let result_budget = normalized_result_budget(max_result_bytes);
    let payload_budget = result_budget.saturating_sub(MODEL_RESULT_ENVELOPE_RESERVE_BYTES);
    let complete = batch_output(
        project,
        requested_count,
        completed.clone(),
        false,
        None,
        None,
    );
    // First evaluate the exact sparse model projection. Canonical search
    // metadata stays intact unless that final applicable projection itself
    // exceeds the response budget.
    if projected_batch_serialized_len(&complete, default_timeouts) <= payload_budget {
        return complete;
    }

    let truncation_reason = if result_budget == MAX_SERIALIZED_OUTPUT_BYTES {
        "hard_result_cap"
    } else {
        "batch_response_budget"
    };
    let base_len = projected_batch_serialized_len(
        &batch_output(
            project,
            requested_count,
            Vec::new(),
            true,
            Some(0),
            Some(truncation_reason),
        ),
        default_timeouts,
    );
    let mut returned = Vec::with_capacity(completed.len());
    let mut returned_item_bytes = 0usize;
    let mut next_index = None;

    for item in completed {
        let index = item["index"].as_u64().unwrap_or(returned.len() as u64) as usize;
        let item_len =
            projected_search_item_len(&item, default_timeouts.get(index).copied().unwrap_or(false));
        let candidate_item_count = returned.len() + 1;
        if projected_batch_len(
            base_len,
            returned_item_bytes.saturating_add(item_len),
            candidate_item_count,
        ) <= payload_budget
        {
            returned_item_bytes = returned_item_bytes.saturating_add(item_len);
            returned.push(item);
            continue;
        }

        // Search batch continuation is query-granular. Backend match order is
        // intentionally unstable, so exposing a partial query and resuming by
        // match position would permit duplicates and gaps across reruns.
        next_index = Some(index);
        break;
    }

    let mut output = batch_output(
        project,
        requested_count,
        returned,
        true,
        next_index,
        Some(truncation_reason),
    );
    while projected_batch_serialized_len(&output, default_timeouts) > payload_budget {
        let Some(items) = output.get_mut("items").and_then(Value::as_array_mut) else {
            break;
        };
        let Some(removed) = items.pop() else {
            break;
        };
        next_index = removed["index"].as_u64().map(|index| index as usize);
        output = batch_output(
            project,
            requested_count,
            items.clone(),
            true,
            next_index,
            Some(truncation_reason),
        );
    }
    output
}

fn failure_reason_code(result: &ToolResult) -> &'static str {
    match result.output.get("code").and_then(Value::as_str) {
        Some("invalid_search_request") => {
            match result.output.get("field").and_then(Value::as_str) {
                Some("pattern") => "invalid_pattern",
                Some("path") => "invalid_path",
                Some("include_globs" | "exclude_globs") => "invalid_glob",
                _ => "invalid_search_request",
            }
        }
        Some("search_timeout") => "timeout",
        Some("search_backend_feature_unavailable") => "search_backend_feature_unavailable",
        Some("search_execution_failed") => "search_execution_failed",
        Some("search_request_dropped") => "search_request_dropped",
        _ if result.output.get("format").and_then(Value::as_str)
            == Some("webcodex.external_provider_error.v1") =>
        {
            "external_provider_error"
        }
        _ => "agent_unavailable",
    }
}

fn batch_failure_stage(result: &ToolResult, broad_reason: &str) -> &'static str {
    match result.output.get("failure_stage").and_then(Value::as_str) {
        Some("request_validation") => "request_validation",
        Some("backend_selection") => "backend_selection",
        Some("backend_protocol") => "backend_protocol",
        Some("backend_execution") => "backend_execution",
        Some("agent_request") => "agent_request",
        Some("agent_execution") => "agent_execution",
        Some("agent_transport") => "agent_transport",
        Some("provider") => "provider",
        Some("local_execution") => "local_execution",
        Some("batch_deadline") => "batch_deadline",
        _ => match broad_reason {
            "invalid_pattern" | "invalid_path" | "invalid_glob" | "invalid_search_request" => {
                "request_validation"
            }
            "search_backend_feature_unavailable" => "backend_selection",
            "search_request_dropped" => "agent_transport",
            "external_provider_error" => "provider",
            "agent_unavailable" => "agent_request",
            "timeout" => "agent_transport",
            _ => "backend_execution",
        },
    }
}

fn batch_failure_detail_code(result: &ToolResult, broad_reason: &'static str) -> &'static str {
    match result.output.get("reason_code").and_then(Value::as_str) {
        Some("invalid_pattern") => "invalid_pattern",
        Some("invalid_path") => "invalid_path",
        Some("invalid_glob") => "invalid_glob",
        Some("invalid_search_request") => "invalid_search_request",
        Some("backend_feature_unavailable") => "backend_feature_unavailable",
        Some("backend_identity_missing") => "backend_identity_missing",
        Some("backend_identity_invalid") => "backend_identity_invalid",
        Some("backend_status_unavailable") => "backend_status_unavailable",
        Some("backend_output_inconsistent") => "backend_output_inconsistent",
        Some("backend_process_failed") => "backend_process_failed",
        Some("agent_request_failed") => "agent_request_failed",
        Some("agent_execution_failed") => "agent_execution_failed",
        Some("search_request_dropped") => "search_request_dropped",
        Some("timeout") => "timeout",
        Some("provider_execution_failed") => "provider_execution_failed",
        Some("provider_protocol_invalid") => "provider_protocol_invalid",
        Some("local_execution_failed") => "local_execution_failed",
        _ => broad_reason,
    }
}

fn copy_safe_batch_failure_metadata(source: &Value, target: &mut Value) {
    if let Some(backend @ ("rg" | "grep" | "native" | "claude_code")) =
        source.get("backend").and_then(Value::as_str)
    {
        target["backend"] = json!(backend);
    }
    if let Some(exit_code) = source.get("exit_code").and_then(Value::as_i64) {
        target["exit_code"] = json!(exit_code);
    }
    if let Some(result_mode @ ("matches" | "files_with_matches" | "count")) =
        source.get("result_mode").and_then(Value::as_str)
    {
        target["result_mode"] = json!(result_mode);
    }
    if let Some(timeout) = source
        .get("effective_timeout_secs")
        .and_then(Value::as_u64)
        .filter(|timeout| (1..=120).contains(timeout))
    {
        target["effective_timeout_secs"] = json!(timeout);
    }
    if let Some(provider_code) =
        source
            .get("provider_code")
            .and_then(Value::as_str)
            .filter(|code| {
                !code.is_empty()
                    && code.len() <= 64
                    && code.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
                    })
            })
    {
        target["provider_code"] = json!(provider_code);
    }
}

fn batch_item(index: usize, mut result: ToolResult) -> Value {
    if result.success {
        if let Some(output) = result.output.as_object_mut() {
            // Project identity and Session/permission metadata are outer-batch
            // concerns. The input index identifies the original pattern.
            for key in [
                "project",
                "pattern",
                "session_recorded",
                "session_id",
                "session_event_id",
                "session_hint",
                "permission",
            ] {
                output.remove(key);
            }
        }
        return json!({
            "index": index,
            "success": true,
            "output": result.output,
            "error": null,
        });
    }

    let reason_code = failure_reason_code(&result);
    let failure_stage = batch_failure_stage(&result, reason_code);
    let detail_code = batch_failure_detail_code(&result, reason_code);
    let mut output = json!({
        "error_kind": "search_project_text_failed",
        // Preserve the established broad batch reason while adding the
        // single-search provenance that explains where and why it failed.
        "reason_code": reason_code,
        "failure_stage": failure_stage,
        "detail_code": detail_code,
        "state_changed": false,
    });
    copy_safe_batch_failure_metadata(&result.output, &mut output);
    json!({
        "index": index,
        "success": false,
        "output": output,
        "error": format!("search_project_text failed: {reason_code}"),
    })
}

pub(crate) fn apply_model_facing_output_budget(
    result: &mut ToolResult,
    default_timeouts: &[bool],
    max_result_bytes: Option<usize>,
) {
    if !result.success {
        return;
    }
    let Some(output) = result.output.as_object() else {
        return;
    };
    let Some(project) = output
        .get("project")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return;
    };
    let Some(requested_count) = output
        .get("requested_count")
        .and_then(Value::as_u64)
        .map(|count| count as usize)
    else {
        return;
    };
    let Some(completed) = output.get("items").and_then(Value::as_array).cloned() else {
        return;
    };

    let budgeted = apply_output_budget(
        &project,
        requested_count,
        completed,
        default_timeouts,
        max_result_bytes,
    );
    let Some(root) = result.output.as_object_mut() else {
        return;
    };
    for key in [
        "project",
        "requested_count",
        "returned_count",
        "succeeded_count",
        "failed_count",
        "items",
        "output_truncated",
        "next_index",
        "truncation_reason",
    ] {
        root.remove(key);
    }
    if let Some(budgeted) = budgeted.as_object() {
        for (key, value) in budgeted {
            root.insert(key.clone(), value.clone());
        }
    }
}

fn final_model_result_len(output: &Value, default_timeouts: &[bool]) -> usize {
    let mut projected = ToolResult::ok(output.clone());
    super::dispatch::sparsify_search_batch_success_for_model(default_timeouts, &mut projected);
    serde_json::to_vec(&projected)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

fn mark_final_hard_cap_truncation(output: &mut Value, next_index: usize) {
    let Some(root) = output.as_object_mut() else {
        return;
    };
    let (returned_count, succeeded_count) = root
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            (
                items.len(),
                items
                    .iter()
                    .filter(|item| item["success"].as_bool() == Some(true))
                    .count(),
            )
        })
        .unwrap_or_default();
    root.insert("returned_count".to_string(), json!(returned_count));
    root.insert("succeeded_count".to_string(), json!(succeeded_count));
    root.insert(
        "failed_count".to_string(),
        json!(returned_count.saturating_sub(succeeded_count)),
    );
    root.insert("output_truncated".to_string(), json!(true));
    root.insert("next_index".to_string(), json!(next_index));
    root.insert("truncation_reason".to_string(), json!("hard_result_cap"));
}

/// Enforce the repository-wide 256 KiB ceiling against the actual final
/// serialized ToolResult, including Session/continuity overlays. Search
/// continuation remains query-granular: whole query items are removed from the
/// end until the fully decorated result fits, and next_index points at the first
/// omitted query.
pub(crate) fn enforce_final_model_facing_hard_cap(
    result: &mut ToolResult,
    default_timeouts: &[bool],
) {
    if !result.success
        || final_model_result_len(&result.output, default_timeouts) <= MAX_SERIALIZED_OUTPUT_BYTES
    {
        return;
    }
    let Some(root) = result.output.as_object() else {
        return;
    };
    if root.get("project").and_then(Value::as_str).is_none()
        || root
            .get("requested_count")
            .and_then(Value::as_u64)
            .is_none()
        || root.get("items").and_then(Value::as_array).is_none()
    {
        return;
    }

    loop {
        let removed_index = {
            let Some(items) = result.output.get_mut("items").and_then(Value::as_array_mut) else {
                return;
            };
            let Some(removed) = items.pop() else {
                return;
            };
            removed["index"].as_u64().unwrap_or(0) as usize
        };
        mark_final_hard_cap_truncation(&mut result.output, removed_index);
        if final_model_result_len(&result.output, default_timeouts) <= MAX_SERIALIZED_OUTPUT_BYTES {
            return;
        }
    }
}

impl ToolRuntime {
    pub(crate) async fn search_project_texts(
        &self,
        project: String,
        queries: Vec<SearchProjectTextsQuery>,
    ) -> ToolResult {
        let resolved = match self.resolve_project_input(&project).await {
            Ok(project) => project,
            Err(error) => return error.into_tool_result(),
        };
        self.search_project_texts_resolved(&resolved, queries).await
    }

    pub(crate) async fn search_project_texts_resolved(
        &self,
        resolved: &ResolvedProject,
        queries: Vec<SearchProjectTextsQuery>,
    ) -> ToolResult {
        if !(1..=MAX_SEARCH_PROJECT_TEXTS_QUERIES).contains(&queries.len()) {
            return ToolResult::err("search_project_texts requires 1 to 8 queries");
        }

        let runtime_project_id = resolved.resolved_id.clone();
        let requested_count = queries.len();
        let deadline = Instant::now() + self.search_project_texts_deadline;
        // Validation, Runner enqueue, and response waiting all happen inside
        // the concurrency slot. A third query cannot enter the Runner queue
        // while two earlier queries still hold their slots.
        let mut completed: Vec<Value> =
            stream::iter(queries.into_iter().enumerate().map(|(index, query)| {
                let project = &resolved.config;
                let output_project = runtime_project_id.as_str();
                async move {
                    let (request, pattern_mode) = search_request_and_pattern_mode(query);
                    let result =
                        match SearchOptions::normalize_with_pattern_mode(request, pattern_mode) {
                            Ok(options) => {
                                let first = self
                                    .search_one_resolved_project_text(
                                        project,
                                        output_project,
                                        options.clone(),
                                        Some(deadline),
                                    )
                                    .await;
                                if retryable_runner_request_failure(&first)
                                    && Instant::now() < deadline
                                {
                                    self.search_one_resolved_project_text(
                                        project,
                                        output_project,
                                        options,
                                        Some(deadline),
                                    )
                                    .await
                                } else {
                                    first
                                }
                            }
                            Err(error) => error.into_tool_result(),
                        };
                    batch_item(index, result)
                }
            }))
            .buffer_unordered(MAX_SEARCH_PROJECT_TEXTS_CONCURRENCY)
            .collect()
            .await;
        completed.sort_by_key(|item| item["index"].as_u64().unwrap_or(u64::MAX));

        ToolResult::ok(batch_output(
            &runtime_project_id,
            requested_count,
            completed,
            false,
            None,
            None,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches_item(index: usize, count: usize, preview_bytes: usize) -> Value {
        let matches = (0..count)
            .map(|match_index| {
                json!({
                    "path": format!("src/{index}-{match_index:03}.rs"),
                    "line": match_index + 1,
                    "preview": format!("m{match_index:03}-{}", "界".repeat(preview_bytes / 3)),
                    "context_before": [],
                    "context_after": [],
                    "read_hint": {
                        "path": format!("src/{index}-{match_index:03}.rs"),
                        "start_line": 1,
                        "limit": 80
                    }
                })
            })
            .collect::<Vec<_>>();
        json!({
            "index": index,
            "success": true,
            "output": {
                "backend": "rg",
                "result_mode": "matches",
                "count": count,
                "matches": matches,
                "truncated": false,
                "truncation_reason": null
            },
            "error": null
        })
    }

    fn default_matches_item(index: usize, count: usize, preview_bytes: usize) -> Value {
        let mut item = matches_item(index, count, preview_bytes);
        let output = item["output"].as_object_mut().unwrap();
        output.insert("path".to_string(), json!("."));
        output.insert("effective_timeout_secs".to_string(), json!(30));
        output.insert("exit_code".to_string(), json!(0));
        output.insert("context_before".to_string(), json!(0));
        output.insert("context_after".to_string(), json!(0));
        item
    }

    #[test]
    fn complete_default_sparse_fit_is_not_preemptively_budget_truncated() {
        let payload_budget =
            DEFAULT_SEARCH_PROJECT_TEXTS_RESULT_BYTES - MODEL_RESULT_ENVELOPE_RESERVE_BYTES;
        let mut selected = None;
        for preview_bytes in 6_000..=9_000 {
            let completed = (0..8)
                .map(|index| default_matches_item(index, 1, preview_bytes))
                .collect::<Vec<_>>();
            let canonical = batch_output("agent:oe:demo", 8, completed.clone(), false, None, None);
            let canonical_bytes = serialized_batch_len(&canonical);
            let sparse_bytes = projected_batch_serialized_len(&canonical, &[true; 8]);
            if canonical_bytes > payload_budget && sparse_bytes <= payload_budget {
                selected = Some((completed, canonical_bytes, sparse_bytes));
                break;
            }
        }
        let (completed, canonical_bytes, sparse_bytes) =
            selected.expect("test fixture must straddle canonical/sparse budget boundary");
        assert!(canonical_bytes > payload_budget);
        assert!(sparse_bytes <= payload_budget);

        let mut result = ToolResult::ok(batch_output(
            "agent:oe:demo",
            8,
            completed.clone(),
            false,
            None,
            None,
        ));
        apply_model_facing_output_budget(&mut result, &[true; 8], None);
        assert_eq!(result.output["output_truncated"], false);
        assert!(result.output["next_index"].is_null());
        assert_eq!(result.output["items"].as_array().unwrap().len(), 8);
        for (actual, expected) in result.output["items"]
            .as_array()
            .unwrap()
            .iter()
            .zip(completed.iter())
        {
            assert_eq!(actual["output"]["matches"], expected["output"]["matches"]);
        }

        super::super::dispatch::sparsify_search_batch_success_for_model(&[true; 8], &mut result);
        assert!(result.output.get("output_truncated").is_none());
        assert!(result.output.get("next_index").is_none());
        assert_eq!(result.output["items"].as_array().unwrap().len(), 8);
    }

    #[test]
    fn output_budget_keeps_whole_items_and_reserves_outer_metadata_space() {
        let item = |index, text: String| {
            json!({
                "index": index,
                "success": true,
                "output": {
                    "backend": "rg",
                    "result_mode": "matches",
                    "count": 1,
                    "matches": [{
                        "path": format!("src/{index}.rs"),
                        "line": 1,
                        "preview": text,
                        "context_before": [],
                        "context_after": [],
                        "read_hint": {
                            "path": format!("src/{index}.rs"),
                            "start_line": 1,
                            "limit": 80
                        }
                    }],
                    "truncated": false,
                    "truncation_reason": null
                },
                "error": null
            })
        };
        let output = apply_output_budget(
            "agent:oe:demo",
            3,
            vec![
                item(0, "x".repeat(120 * 1024)),
                item(1, "y".repeat(120 * 1024)),
                item(2, "z".repeat(120 * 1024)),
            ],
            &[false, false, false],
            Some(MAX_SERIALIZED_OUTPUT_BYTES),
        );
        assert_eq!(output["returned_count"], 2);
        assert_eq!(output["next_index"], 2);
        assert_eq!(output["output_truncated"], true);

        let mut result = ToolResult::ok(output);
        result.output["session_hint"] = json!({
            "has_open_messages": true,
            "open_counts": {
                "guidance": u64::MAX,
                "question": u64::MAX,
                "todo": u64::MAX,
                "risk": u64::MAX
            },
            "highest_priority": "high",
            "suggested_next_tool": "session_discussion_summary"
        });
        assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_SERIALIZED_OUTPUT_BYTES);
    }

    #[test]
    fn single_query_soft_budget_omits_whole_item_then_larger_budget_returns_it() {
        let completed = vec![default_matches_item(0, 120, 900)];
        let default = apply_output_budget("agent:oe:demo", 1, completed.clone(), &[true], None);
        assert_eq!(default["returned_count"], 0);
        assert!(default["items"].as_array().unwrap().is_empty());
        assert_eq!(default["next_index"], 0);
        assert_eq!(default["output_truncated"], true);
        assert_eq!(default["truncation_reason"], "batch_response_budget");

        let large = apply_output_budget(
            "agent:oe:demo",
            1,
            completed,
            &[true],
            Some(MAX_SERIALIZED_OUTPUT_BYTES),
        );
        assert_eq!(large["output_truncated"], false);
        assert!(large["next_index"].is_null());
        assert_eq!(large["returned_count"], 1);
        assert_eq!(
            large["items"][0]["output"]["matches"]
                .as_array()
                .unwrap()
                .len(),
            120
        );
    }

    #[test]
    fn whole_query_continuation_is_independent_of_backend_match_order() {
        let first_query = default_matches_item(0, 1, 100);
        let omitted_query = default_matches_item(1, 90, 900);
        let first = apply_output_budget(
            "agent:oe:demo",
            2,
            vec![first_query.clone(), omitted_query],
            &[true, true],
            None,
        );
        assert_eq!(first["returned_count"], 1);
        assert_eq!(first["items"][0], first_query);
        assert_eq!(first["next_index"], 1);
        assert_eq!(first["output_truncated"], true);

        // A suffix rerun is a fresh query execution. Deliberately reverse its
        // match order to prove continuation does not stitch by match position.
        let mut rerun = default_matches_item(0, 90, 900);
        rerun["output"]["matches"].as_array_mut().unwrap().reverse();
        let rerun_matches = rerun["output"]["matches"].clone();
        let continuation = apply_output_budget(
            "agent:oe:demo",
            1,
            vec![rerun],
            &[true],
            Some(MAX_SERIALIZED_OUTPUT_BYTES),
        );
        assert_eq!(continuation["output_truncated"], false);
        assert_eq!(continuation["returned_count"], 1);
        assert_eq!(continuation["items"][0]["output"]["matches"], rerun_matches);
    }

    #[test]
    fn fitting_producer_truncation_remains_independent_of_batch_budget() {
        for reason in ["limit", "output_bytes"] {
            let mut original = matches_item(0, 2, 100);
            original["output"]["truncated"] = json!(true);
            original["output"]["truncation_reason"] = json!(reason);
            let expected = original.clone();
            let output = apply_output_budget("agent:oe:demo", 1, vec![original], &[false], None);
            assert_eq!(output["output_truncated"], false);
            assert!(output["next_index"].is_null());
            assert_eq!(output["items"][0], expected);
        }
    }

    #[test]
    fn hard_cap_pressure_omits_oversized_query_without_partial_matches() {
        let output = apply_output_budget(
            "agent:oe:demo",
            1,
            vec![matches_item(0, 199, 2_000)],
            &[false],
            Some(MAX_SERIALIZED_OUTPUT_BYTES),
        );
        assert_eq!(output["returned_count"], 0);
        assert!(output["items"].as_array().unwrap().is_empty());
        assert_eq!(output["next_index"], 0);
        assert_eq!(output["output_truncated"], true);
        assert_eq!(output["truncation_reason"], "hard_result_cap");
    }

    #[test]
    fn search_result_budget_clamps_to_existing_hard_cap() {
        assert_eq!(
            normalized_result_budget(Some(MAX_SERIALIZED_OUTPUT_BYTES * 2)),
            MAX_SERIALIZED_OUTPUT_BYTES
        );
    }
}
