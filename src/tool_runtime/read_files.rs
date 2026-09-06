//! Bounded multi-file reads built from the canonical single-file read core.

use super::project_resolution::ResolvedProject;
use super::{ReadFilesItem, ToolCall, ToolResult, ToolRuntime};
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::Instant;
use webcodex_workspace::file_read_normalize::MODEL_RESULT_ENVELOPE_RESERVE_BYTES;
use webcodex_workspace::file_read_range::MAX_SERIALIZED_OUTPUT_BYTES;

pub(crate) const MAX_READ_FILES_ITEMS: usize = 8;
pub(crate) const MAX_READ_FILES_CONCURRENCY: usize = 4;
pub(crate) const DEFAULT_READ_FILES_DEADLINE: Duration = Duration::from_secs(30);
pub(crate) use webcodex_core::runtime_contract::{
    DEFAULT_READ_FILES_RESULT_BYTES, MIN_READ_FILES_RESULT_BYTES,
};

/// Read request facts captured before the ToolCall is moved into execution.
/// Canonical read results remain independent of this projection; these facts
/// exist only so a final model-facing partial result can provide a directly
/// reusable next call without inventing a new cursor.
#[derive(Clone, Debug)]
pub(crate) enum ReadModelProjection {
    None,
    Single {
        project: String,
        path: String,
        session_id: Option<String>,
        with_line_numbers: Option<bool>,
    },
    Batch {
        project: String,
        items: Vec<ReadFilesItem>,
        session_id: Option<String>,
        with_line_numbers: Option<bool>,
        max_result_bytes: Option<usize>,
    },
}

impl ReadModelProjection {
    pub(crate) fn capture(call: &ToolCall) -> Self {
        match call {
            ToolCall::ReadFile {
                project,
                path,
                session_id,
                with_line_numbers,
                ..
            } => Self::Single {
                project: project.clone(),
                path: path.clone(),
                session_id: session_id.clone(),
                with_line_numbers: *with_line_numbers,
            },
            ToolCall::ReadFiles {
                project,
                items,
                session_id,
                with_line_numbers,
                max_result_bytes,
            } => Self::Batch {
                project: project.clone(),
                items: items.clone(),
                session_id: session_id.clone(),
                with_line_numbers: *with_line_numbers,
                max_result_bytes: *max_result_bytes,
            },
            _ => Self::None,
        }
    }

    /// Replace shorthand with the exact Project identity selected by the same
    /// authoritative resolver pass used for this call. Recovery must not
    /// re-enter shorthand resolution and retarget after registry churn.
    pub(crate) fn bind_resolved_project(&mut self, resolved: Option<&ResolvedProject>) {
        let Some(resolved) = resolved else {
            return;
        };
        match self {
            Self::Single { project, .. } | Self::Batch { project, .. } => {
                *project = resolved.resolved_id.clone();
            }
            Self::None => {}
        }
    }
}

fn read_file_suggested_arguments(
    project: &str,
    path: &str,
    start_line: usize,
    limit: usize,
    session_id: Option<&str>,
    with_line_numbers: Option<bool>,
) -> Value {
    let mut arguments = json!({
        "project": project,
        "path": path,
        "start_line": start_line,
        "limit": limit,
    });
    if let Some(session_id) = session_id {
        arguments["session_id"] = json!(session_id);
    }
    if let Some(with_line_numbers) = with_line_numbers {
        arguments["with_line_numbers"] = json!(with_line_numbers);
    }
    arguments
}

fn read_range_continuation(
    project: &str,
    path: &str,
    output: &serde_json::Map<String, Value>,
    session_id: Option<&str>,
    with_line_numbers: Option<bool>,
) -> Option<Value> {
    if output.get("has_more").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let next_start_line = output.get("next_start_line")?.as_u64()? as usize;
    let source_sha256 = output.get("sha256")?.as_str()?;
    let total_lines = output.get("total_lines")?.as_u64()? as usize;
    let remaining_lines = total_lines
        .saturating_sub(next_start_line)
        .saturating_add(1);
    if remaining_lines == 0 {
        return None;
    }
    let requested_limit = output
        .get("budget_next_limit")
        .and_then(Value::as_u64)
        .or_else(|| output.get("limit").and_then(Value::as_u64))?
        as usize;
    let limit = requested_limit.min(remaining_lines).max(1);
    Some(json!({
        "kind": "read_range",
        "safe_cursor": true,
        "source_sha256": source_sha256,
        "snapshot_stable": false,
        "suggested_call": {
            "tool": "read_file",
            "arguments": read_file_suggested_arguments(
                project,
                path,
                next_start_line,
                limit,
                session_id,
                with_line_numbers,
            )
        }
    }))
}

fn add_item_read_continuation(
    item: &mut Value,
    project: &str,
    session_id: Option<&str>,
    with_line_numbers: Option<bool>,
) {
    if item.get("success").and_then(Value::as_bool) != Some(true) {
        return;
    }
    let Some(path) = item.get("path").and_then(Value::as_str).map(str::to_string) else {
        return;
    };
    let continuation = item
        .get("output")
        .and_then(Value::as_object)
        .and_then(|output| {
            read_range_continuation(project, &path, output, session_id, with_line_numbers)
        });
    if let (Some(continuation), Some(item)) = (continuation, item.as_object_mut()) {
        item.insert("continuation".to_string(), continuation);
    }
}

fn read_files_suggested_arguments(
    project: &str,
    items: &[ReadFilesItem],
    session_id: Option<&str>,
    with_line_numbers: Option<bool>,
    max_result_bytes: Option<usize>,
) -> Value {
    let suggested_items = items
        .iter()
        .map(|item| {
            let mut suggested = json!({"path": item.path});
            if let Some(start_line) = item.start_line {
                suggested["start_line"] = json!(start_line);
            }
            if let Some(limit) = item.limit {
                suggested["limit"] = json!(limit);
            }
            suggested
        })
        .collect::<Vec<_>>();
    let mut arguments = json!({
        "project": project,
        "items": suggested_items,
    });
    if let Some(session_id) = session_id {
        arguments["session_id"] = json!(session_id);
    }
    if let Some(with_line_numbers) = with_line_numbers {
        arguments["with_line_numbers"] = json!(with_line_numbers);
    }
    if let Some(max_result_bytes) = max_result_bytes {
        arguments["max_result_bytes"] = json!(max_result_bytes);
    }
    arguments
}

fn add_batch_read_continuation(
    output: &mut serde_json::Map<String, Value>,
    project: &str,
    original_items: &[ReadFilesItem],
    session_id: Option<&str>,
    with_line_numbers: Option<bool>,
    max_result_bytes: Option<usize>,
) {
    if output.get("output_truncated").and_then(Value::as_bool) != Some(true) {
        return;
    }
    let Some(next_index) = output.get("next_index").and_then(Value::as_u64) else {
        return;
    };
    let next_index = next_index as usize;
    if next_index >= original_items.len() {
        return;
    }
    let returned_items = output.get("items").and_then(Value::as_array);
    let partial_current = returned_items.is_some_and(|items| {
        items.iter().any(|item| {
            item.get("index").and_then(Value::as_u64) == Some(next_index as u64)
                && item.get("success").and_then(Value::as_bool) == Some(true)
                && item
                    .get("output")
                    .and_then(|output| output.get("budget_truncated"))
                    .and_then(Value::as_bool)
                    == Some(true)
        })
    });
    let first_unreturned_index = next_index.saturating_add(usize::from(partial_current));

    // If the primary response budget could not return even part of its first
    // item, replaying the same request at the same budget cannot make progress.
    // This is parameter refinement rather than a safe cursor: recommend the
    // existing hard maximum, never a value above it.
    if next_index == 0
        && returned_items.is_some_and(Vec::is_empty)
        && max_result_bytes.unwrap_or(DEFAULT_READ_FILES_RESULT_BYTES) < MAX_SERIALIZED_OUTPUT_BYTES
    {
        output.insert(
            "continuation".to_string(),
            json!({
                "kind": "increase_result_budget",
                "safe_cursor": false,
                "next_index": 0,
                "suggested_max_result_bytes": MAX_SERIALIZED_OUTPUT_BYTES,
                "suggested_call": {
                    "tool": "read_files",
                    "arguments": read_files_suggested_arguments(
                        project,
                        original_items,
                        session_id,
                        with_line_numbers,
                        Some(MAX_SERIALIZED_OUTPUT_BYTES),
                    )
                }
            }),
        );
        return;
    }

    if first_unreturned_index >= original_items.len() {
        return;
    }
    let remaining = &original_items[first_unreturned_index..];
    output.insert(
        "continuation".to_string(),
        json!({
            "kind": "batch_items",
            "safe_cursor": true,
            "next_index": first_unreturned_index,
            "recommended_order": if partial_current { "after_partial_item" } else { "next" },
            "suggested_call": {
                "tool": "read_files",
                "arguments": read_files_suggested_arguments(
                    project,
                    remaining,
                    session_id,
                    with_line_numbers,
                    max_result_bytes,
                )
            }
        }),
    );
}

/// Add actionable model-only continuation metadata to successful reads. The
/// source cursor remains positional: `source_sha256` identifies the file that
/// produced the current range, while `snapshot_stable=false` makes explicit
/// that a later positional read must compare its newly returned full-file hash
/// before the model treats both ranges as one unchanged snapshot.
pub(crate) fn add_actionable_read_continuations(
    projection: &ReadModelProjection,
    result: &mut ToolResult,
) {
    if !result.success {
        return;
    }
    match projection {
        ReadModelProjection::None => {}
        ReadModelProjection::Single {
            project,
            path,
            session_id,
            with_line_numbers,
        } => {
            let continuation = result.output.as_object().and_then(|output| {
                read_range_continuation(
                    project,
                    path,
                    output,
                    session_id.as_deref(),
                    *with_line_numbers,
                )
            });
            if let (Some(continuation), Some(output)) =
                (continuation, result.output.as_object_mut())
            {
                output.insert("continuation".to_string(), continuation);
            }
        }
        ReadModelProjection::Batch {
            project,
            items: original_items,
            session_id,
            with_line_numbers,
            max_result_bytes,
        } => {
            if let Some(items) = result.output.get_mut("items").and_then(Value::as_array_mut) {
                for item in items {
                    add_item_read_continuation(
                        item,
                        project,
                        session_id.as_deref(),
                        *with_line_numbers,
                    );
                }
            }
            if let Some(output) = result.output.as_object_mut() {
                add_batch_read_continuation(
                    output,
                    project,
                    original_items,
                    session_id.as_deref(),
                    *with_line_numbers,
                    *max_result_bytes,
                );
            }
        }
    }
}

fn normalized_result_budget(max_result_bytes: Option<usize>) -> usize {
    max_result_bytes
        .unwrap_or(DEFAULT_READ_FILES_RESULT_BYTES)
        .clamp(MIN_READ_FILES_RESULT_BYTES, MAX_SERIALIZED_OUTPUT_BYTES)
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

fn projected_batch_serialized_len(output: &Value, projection: &ReadModelProjection) -> usize {
    let mut projected = ToolResult::ok(output.clone());
    add_actionable_read_continuations(projection, &mut projected);
    super::dispatch::sparsify_complete_read_success("read_files", &mut projected);
    serde_json::to_vec(&projected)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

fn projected_read_item_len(item: &Value, projection: &ReadModelProjection) -> usize {
    let mut projected = item.clone();
    if let ReadModelProjection::Batch {
        project,
        session_id,
        with_line_numbers,
        ..
    } = projection
    {
        add_item_read_continuation(
            &mut projected,
            project,
            session_id.as_deref(),
            *with_line_numbers,
        );
    }
    if projected["success"].as_bool() == Some(true) {
        let outer_path = projected
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let (Some(outer_path), Some(output)) = (
            outer_path,
            projected.get_mut("output").and_then(Value::as_object_mut),
        ) {
            super::dispatch::sparsify_complete_file_read_output(output, Some(&outer_path));
        }
    }
    serialized_value_len(&projected)
}

fn projected_batch_len(base_len: usize, item_bytes: usize, item_count: usize) -> usize {
    base_len
        .saturating_add(item_bytes)
        .saturating_add(item_count.saturating_sub(1))
}

fn truncate_read_item(item: &Value, keep_lines: usize) -> Option<Value> {
    if item["success"].as_bool() != Some(true) || keep_lines == 0 {
        return None;
    }
    let output = item.get("output")?.as_object()?;
    let returned_lines = output.get("returned_lines")?.as_u64()? as usize;
    if keep_lines >= returned_lines || returned_lines <= 1 {
        return None;
    }
    let start_line = output.get("start_line")?.as_u64()? as usize;
    let text = output.get("text")?.as_str()?;
    let lines = text.split('\n').collect::<Vec<_>>();
    if lines.len() != returned_lines {
        return None;
    }

    let mut projected = item.clone();
    let projected_output = projected.get_mut("output")?.as_object_mut()?;
    projected_output.insert("text".to_string(), json!(lines[..keep_lines].join("\n")));
    projected_output.insert("returned_lines".to_string(), json!(keep_lines));
    projected_output.insert(
        "end_line".to_string(),
        json!(start_line.saturating_add(keep_lines).saturating_sub(1)),
    );
    projected_output.insert("has_more".to_string(), json!(true));
    projected_output.insert(
        "next_start_line".to_string(),
        json!(start_line.saturating_add(keep_lines)),
    );
    projected_output.insert("budget_truncated".to_string(), json!(true));
    let existing_budget_next_limit = output
        .get("budget_next_limit")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    projected_output.insert(
        "budget_next_limit".to_string(),
        json!(returned_lines
            .saturating_sub(keep_lines)
            .saturating_add(existing_budget_next_limit)),
    );
    Some(projected)
}

fn truncate_read_item_to_fit(
    item: &Value,
    max_item_bytes: usize,
    projection: &ReadModelProjection,
) -> Option<Value> {
    let returned_lines = item.get("output")?.get("returned_lines")?.as_u64()? as usize;
    if returned_lines <= 1 {
        return None;
    }

    let mut low = 1usize;
    let mut high = returned_lines - 1;
    let mut best = None;
    while low <= high {
        let keep = low + (high - low) / 2;
        let Some(candidate) = truncate_read_item(item, keep) else {
            break;
        };
        if projected_read_item_len(&candidate, projection) <= max_item_bytes {
            best = Some(candidate);
            low = keep.saturating_add(1);
        } else {
            high = keep.saturating_sub(1);
        }
    }
    best
}

fn apply_output_budget(
    project: &str,
    requested_count: usize,
    completed: Vec<Value>,
    max_result_bytes: Option<usize>,
    projection: &ReadModelProjection,
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
    // Budget the shape the model would actually receive after the existing
    // sparse projection. The canonical representation remains available for
    // Session recording and for any partial-item cursor construction below.
    if projected_batch_serialized_len(&complete, projection) <= payload_budget {
        return complete;
    }

    let truncation_reason = if result_budget == MAX_SERIALIZED_OUTPUT_BYTES {
        "hard_result_cap"
    } else {
        "batch_response_budget"
    };
    // The truncated empty shape fixes all outer-field byte costs up front.
    // Counts and indices are single digits for this 1..=8 batch, so each item
    // can then be accounted exactly by its own serialized size plus one comma.
    let base_len = projected_batch_serialized_len(
        &batch_output(
            project,
            requested_count,
            Vec::new(),
            true,
            Some(0),
            Some(truncation_reason),
        ),
        projection,
    );
    let mut returned = Vec::with_capacity(completed.len());
    let mut returned_item_bytes = 0usize;
    let mut next_index = None;

    for item in completed {
        let index = item["index"].as_u64().unwrap_or(returned.len() as u64) as usize;
        let item_len = projected_read_item_len(&item, projection);
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

        let separator_bytes = usize::from(!returned.is_empty());
        let max_item_bytes = payload_budget
            .saturating_sub(base_len)
            .saturating_sub(returned_item_bytes)
            .saturating_sub(separator_bytes);
        if let Some(partial) = truncate_read_item_to_fit(&item, max_item_bytes, projection) {
            returned.push(partial);
        }
        // A partial current item resumes from its next_start_line, while an
        // omitted item resumes from its original range. In both cases the
        // existing next_index can deterministically point at this same item.
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
    // Defensive exact serialization fallback. Normal accounting above is O(n)
    // plus an O(log lines) partial-item search; this loop should never execute
    // unless a future outer-field change invalidates the fixed-size arithmetic.
    while projected_batch_serialized_len(&output, projection) > payload_budget {
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

pub(crate) fn apply_model_facing_output_budget(
    result: &mut ToolResult,
    max_result_bytes: Option<usize>,
    projection: &ReadModelProjection,
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
        max_result_bytes,
        projection,
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

fn final_model_result_len(output: &Value, projection: &ReadModelProjection) -> usize {
    let mut projected = ToolResult::ok(output.clone());
    add_actionable_read_continuations(projection, &mut projected);
    super::dispatch::sparsify_complete_read_success("read_files", &mut projected);
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
/// serialized ToolResult, including Session/continuity overlays. The primary
/// batch budget remains independent; this pass only removes/shortens read body
/// content when the fully decorated response would otherwise violate the hard
/// cap.
///
/// This expects the canonical batch envelope (before complete-success sparse
/// projection), so project/count/continuation metadata can remain truthful when
/// final hard-cap pressure turns a previously complete response into a partial
/// one.
pub(crate) fn enforce_final_model_facing_hard_cap(
    result: &mut ToolResult,
    projection: &ReadModelProjection,
) {
    if !result.success
        || final_model_result_len(&result.output, projection) <= MAX_SERIALIZED_OUTPUT_BYTES
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
        let Some(items) = result.output.get("items").and_then(Value::as_array) else {
            return;
        };
        let Some(last) = items.last() else {
            return;
        };
        let index = last["index"].as_u64().unwrap_or(0) as usize;

        // Preserve as many whole source lines as possible from the last success
        // item. Measuring the complete decorated ToolResult makes this exact and
        // naturally accounts for recovery/handoff/attention bytes.
        if last["success"].as_bool() == Some(true) {
            let returned_lines = last
                .get("output")
                .and_then(|output| output.get("returned_lines"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            if returned_lines > 1 {
                let mut low = 1usize;
                let mut high = returned_lines - 1;
                let mut best = None;
                while low <= high {
                    let keep = low + (high - low) / 2;
                    let Some(partial) = truncate_read_item(last, keep) else {
                        break;
                    };
                    let mut candidate = result.output.clone();
                    if let Some(candidate_items) =
                        candidate.get_mut("items").and_then(Value::as_array_mut)
                    {
                        let last_index = candidate_items.len() - 1;
                        candidate_items[last_index] = partial;
                    }
                    mark_final_hard_cap_truncation(&mut candidate, index);
                    if final_model_result_len(&candidate, projection) <= MAX_SERIALIZED_OUTPUT_BYTES
                    {
                        best = Some(candidate);
                        low = keep.saturating_add(1);
                    } else {
                        high = keep.saturating_sub(1);
                    }
                }
                if let Some(best) = best {
                    result.output = best;
                    return;
                }
            }
        }

        let removed_index = {
            let Some(items) = result.output.get_mut("items").and_then(Value::as_array_mut) else {
                return;
            };
            items
                .pop()
                .and_then(|removed| removed["index"].as_u64())
                .unwrap_or(index as u64) as usize
        };
        mark_final_hard_cap_truncation(&mut result.output, removed_index);
        if final_model_result_len(&result.output, projection) <= MAX_SERIALIZED_OUTPUT_BYTES {
            return;
        }
    }
}

impl ToolRuntime {
    pub(crate) async fn read_files(
        &self,
        project: String,
        items: Vec<ReadFilesItem>,
        with_line_numbers: Option<bool>,
    ) -> ToolResult {
        let resolved = match self.resolve_project_input(&project).await {
            Ok(project) => project,
            Err(error) => return ToolResult::err(error),
        };
        self.read_files_resolved(&resolved, items, with_line_numbers)
            .await
    }

    pub(crate) async fn read_files_resolved(
        &self,
        resolved: &ResolvedProject,
        items: Vec<ReadFilesItem>,
        with_line_numbers: Option<bool>,
    ) -> ToolResult {
        if !(1..=MAX_READ_FILES_ITEMS).contains(&items.len())
            || items.iter().any(|item| item.path.trim().is_empty())
        {
            return ToolResult::err("read_files requires 1 to 8 items with non-empty paths");
        }

        let runtime_project_id = resolved.resolved_id.clone();
        let requested_count = items.len();
        let with_line_numbers = with_line_numbers.unwrap_or(false);
        let deadline = Instant::now() + self.read_files_deadline;

        // The concurrency slot covers validation, enqueue, and response wait.
        // No request can reach the Runner until its future is polled by
        // `buffer_unordered`, so at most four file reads are actually in flight.
        let mut completed: Vec<Value> =
            stream::iter(items.into_iter().enumerate().map(|(index, item)| {
                let project = &resolved.config;
                async move {
                    let path = item.path;
                    let result = self
                        .read_one_resolved_project_file(
                            project,
                            path.clone(),
                            item.start_line,
                            item.limit,
                            with_line_numbers,
                            deadline,
                        )
                        .await;
                    json!({
                        "index": index,
                        "path": path,
                        "success": result.success,
                        "output": result.output,
                        "error": result.error,
                    })
                }
            }))
            .buffer_unordered(MAX_READ_FILES_CONCURRENCY)
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

    fn batch_projection(count: usize, max_result_bytes: Option<usize>) -> ReadModelProjection {
        ReadModelProjection::Batch {
            project: "agent:oe:demo".to_string(),
            session_id: None,
            items: (0..count)
                .map(|index| ReadFilesItem {
                    path: format!("src/{index}.rs"),
                    start_line: None,
                    limit: None,
                })
                .collect(),
            with_line_numbers: None,
            max_result_bytes,
        }
    }

    fn ranged_item(index: usize, start_line: usize, lines: &[String]) -> Value {
        let returned_lines = lines.len();
        json!({
            "index": index,
            "path": format!("src/{index}.rs"),
            "success": true,
            "output": {
                "text": lines.join("\n"),
                "format": "plain",
                "path": format!("src/{index}.rs"),
                "sha256": "c".repeat(64),
                "start_line": start_line,
                "limit": returned_lines,
                "total_lines": start_line + returned_lines - 1,
                "returned_lines": returned_lines,
                "end_line": start_line + returned_lines - 1,
                "has_more": false,
                "next_start_line": null
            },
            "error": null
        })
    }

    fn default_complete_item(index: usize, lines: &[String]) -> Value {
        let returned_lines = lines.len();
        let default_limit =
            webcodex_workspace::file_read_range::EffectiveRange::new(None, None).limit;
        json!({
            "index": index,
            "path": format!("src/{index}.rs"),
            "success": true,
            "output": {
                "text": lines.join("\n"),
                "format": "plain",
                "path": format!("src/{index}.rs"),
                "sha256": "d".repeat(64),
                "start_line": 1,
                "limit": default_limit,
                "total_lines": returned_lines,
                "returned_lines": returned_lines,
                "end_line": if returned_lines == 0 { Value::Null } else { json!(returned_lines) },
                "has_more": false,
                "next_start_line": null
            },
            "error": null
        })
    }

    #[test]
    fn complete_default_sparse_fit_is_not_preemptively_budget_truncated() {
        let payload_budget = DEFAULT_READ_FILES_RESULT_BYTES - MODEL_RESULT_ENVELOPE_RESERVE_BYTES;
        let mut selected = None;
        for text_bytes in 6_000..=9_000 {
            let completed = (0..8)
                .map(|index| default_complete_item(index, &["x".repeat(text_bytes)]))
                .collect::<Vec<_>>();
            let canonical = batch_output("agent:oe:demo", 8, completed.clone(), false, None, None);
            let canonical_bytes = serialized_batch_len(&canonical);
            let sparse_bytes =
                projected_batch_serialized_len(&canonical, &batch_projection(8, None));
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
        apply_model_facing_output_budget(&mut result, None, &batch_projection(8, None));
        assert_eq!(result.output["output_truncated"], false);
        assert!(result.output["next_index"].is_null());
        assert_eq!(result.output["items"].as_array().unwrap().len(), 8);
        for (actual, expected) in result.output["items"]
            .as_array()
            .unwrap()
            .iter()
            .zip(completed.iter())
        {
            assert_eq!(actual["output"]["text"], expected["output"]["text"]);
        }

        super::super::dispatch::sparsify_complete_read_success("read_files", &mut result);
        assert!(result.output.get("output_truncated").is_none());
        assert!(result.output.get("next_index").is_none());
        assert_eq!(result.output["items"].as_array().unwrap().len(), 8);
    }

    #[test]
    fn output_budget_keeps_whole_items_and_points_at_first_omitted_index() {
        let item = |index, text: String| {
            json!({
                "index": index,
                "path": format!("src/{index}.rs"),
                "success": true,
                "output": {
                    "text": text,
                    "format": "plain",
                    "path": format!("src/{index}.rs"),
                    "sha256": "a".repeat(64),
                    "start_line": 1,
                    "limit": 1,
                    "total_lines": 1,
                    "returned_lines": 1,
                    "end_line": 1,
                    "has_more": false,
                    "next_start_line": null
                },
                "error": null
            })
        };
        let projection = batch_projection(2, Some(MAX_SERIALIZED_OUTPUT_BYTES));
        let output = apply_output_budget(
            "agent:oe:demo",
            2,
            vec![
                item(0, "x".repeat(140 * 1024)),
                item(1, "y".repeat(140 * 1024)),
            ],
            Some(MAX_SERIALIZED_OUTPUT_BYTES),
            &projection,
        );
        assert_eq!(output["returned_count"], 1);
        assert_eq!(output["output_truncated"], true);
        assert_eq!(output["next_index"], 1);
        assert_eq!(output["items"].as_array().unwrap().len(), 1);
        let serialized = serde_json::to_vec(&ToolResult::ok(output.clone())).unwrap();
        assert!(serialized.len() <= MAX_SERIALIZED_OUTPUT_BYTES);
        let mut model = ToolResult::ok(output);
        add_actionable_read_continuations(&projection, &mut model);
        super::super::dispatch::sparsify_complete_read_success("read_files", &mut model);
        let serialized = serde_json::to_vec(&model).unwrap();
        assert!(
            serialized.len() <= MAX_SERIALIZED_OUTPUT_BYTES,
            "actionable continuation must remain inside the 256 KiB hard cap: {} bytes",
            serialized.len()
        );
    }

    #[test]
    fn omitted_batch_items_have_reusable_sliced_read_files_call() {
        let first = vec!["x".repeat(140 * 1024)];
        let second = vec!["y".repeat(140 * 1024)];
        let third = vec!["z".to_string()];
        let mut projection = batch_projection(3, Some(MAX_SERIALIZED_OUTPUT_BYTES));
        if let ReadModelProjection::Batch { session_id, .. } = &mut projection {
            *session_id = Some("wc_sess_batch_recovery".to_string());
        }
        let output = apply_output_budget(
            "agent:oe:demo",
            3,
            vec![
                ranged_item(0, 1, &first),
                ranged_item(1, 1, &second),
                ranged_item(2, 1, &third),
            ],
            Some(MAX_SERIALIZED_OUTPUT_BYTES),
            &projection,
        );
        assert_eq!(output["output_truncated"], true);
        assert_eq!(output["next_index"], 1);
        assert_eq!(output["items"].as_array().unwrap().len(), 1);

        let mut model = ToolResult::ok(output);
        add_actionable_read_continuations(&projection, &mut model);
        let continuation = &model.output["continuation"];
        assert_eq!(continuation["kind"], "batch_items");
        assert_eq!(continuation["safe_cursor"], true);
        assert_eq!(continuation["next_index"], 1);
        assert_eq!(continuation["recommended_order"], "next");
        let suggested = &continuation["suggested_call"];
        assert_eq!(suggested["tool"], "read_files");
        assert_eq!(
            suggested["arguments"]["session_id"],
            "wc_sess_batch_recovery"
        );
        assert!(suggested["arguments"].get("next_index").is_none());
        assert_eq!(
            suggested["arguments"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["path"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["src/1.rs", "src/2.rs"]
        );
        let next = ToolCall::from_tool_name(
            suggested["tool"].as_str().unwrap(),
            suggested["arguments"].clone(),
        )
        .expect("batch continuation suggested_call must parse");
        assert!(matches!(
            next,
            ToolCall::ReadFiles {
                ref items,
                session_id: Some(ref next_session_id),
                max_result_bytes: Some(bytes),
                ..
            } if bytes == MAX_SERIALIZED_OUTPUT_BYTES
                && next_session_id == "wc_sess_batch_recovery"
                && items.iter().map(|item| item.path.as_str()).collect::<Vec<_>>()
                    == vec!["src/1.rs", "src/2.rs"]
        ));
    }

    #[test]
    fn partial_item_and_later_batch_items_expose_distinct_ordered_recovery() {
        let lines = (0..900)
            .map(|index| format!("第{index:04}行-{}", "界".repeat(40)))
            .collect::<Vec<_>>();
        let later = vec!["later".to_string()];
        let last = vec!["last".to_string()];
        let projection = batch_projection(3, None);
        let output = apply_output_budget(
            "agent:oe:demo",
            3,
            vec![
                default_complete_item(0, &lines),
                ranged_item(1, 1, &later),
                ranged_item(2, 1, &last),
            ],
            None,
            &projection,
        );
        // Canonical compatibility remains unchanged: the raw batch next_index
        // still identifies the partial current item.
        assert_eq!(output["next_index"], 0);
        assert_eq!(output["items"].as_array().unwrap().len(), 1);
        assert_eq!(output["items"][0]["output"]["budget_truncated"], true);

        let mut model = ToolResult::ok(output);
        add_actionable_read_continuations(&projection, &mut model);
        let item_continuation = &model.output["items"][0]["continuation"];
        assert_eq!(item_continuation["kind"], "read_range");
        assert_eq!(item_continuation["safe_cursor"], true);
        let expected_next_start = model.output["items"][0]["output"]["next_start_line"]
            .as_u64()
            .unwrap();
        let expected_limit = model.output["items"][0]["output"]["budget_next_limit"]
            .as_u64()
            .unwrap();
        assert_eq!(
            item_continuation["suggested_call"]["arguments"]["start_line"],
            expected_next_start
        );
        assert_eq!(
            item_continuation["suggested_call"]["arguments"]["limit"],
            expected_limit
        );
        ToolCall::from_tool_name(
            item_continuation["suggested_call"]["tool"]
                .as_str()
                .unwrap(),
            item_continuation["suggested_call"]["arguments"].clone(),
        )
        .expect("partial item continuation must parse");

        let batch_continuation = &model.output["continuation"];
        assert_eq!(batch_continuation["kind"], "batch_items");
        assert_eq!(batch_continuation["safe_cursor"], true);
        assert_eq!(batch_continuation["next_index"], 1);
        assert_eq!(
            batch_continuation["recommended_order"],
            "after_partial_item"
        );
        let suggested = &batch_continuation["suggested_call"];
        assert!(suggested["arguments"].get("next_index").is_none());
        assert_eq!(
            suggested["arguments"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["path"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["src/1.rs", "src/2.rs"]
        );
        ToolCall::from_tool_name(
            suggested["tool"].as_str().unwrap(),
            suggested["arguments"].clone(),
        )
        .expect("later batch continuation must parse");
    }

    #[test]
    fn zero_progress_primary_budget_recommends_bounded_budget_refinement() {
        let huge_line = vec!["x".repeat(90 * 1024)];
        let projection = batch_projection(1, None);
        let output = apply_output_budget(
            "agent:oe:demo",
            1,
            vec![ranged_item(0, 1, &huge_line)],
            None,
            &projection,
        );
        assert_eq!(output["output_truncated"], true);
        assert_eq!(output["next_index"], 0);
        assert!(output["items"].as_array().unwrap().is_empty());

        let mut model = ToolResult::ok(output);
        add_actionable_read_continuations(&projection, &mut model);
        let continuation = &model.output["continuation"];
        assert_eq!(continuation["kind"], "increase_result_budget");
        assert_eq!(continuation["safe_cursor"], false);
        assert_eq!(continuation["next_index"], 0);
        assert_eq!(
            continuation["suggested_max_result_bytes"],
            MAX_SERIALIZED_OUTPUT_BYTES
        );
        let suggested = &continuation["suggested_call"];
        assert_eq!(
            suggested["arguments"]["max_result_bytes"],
            MAX_SERIALIZED_OUTPUT_BYTES
        );
        let next = ToolCall::from_tool_name(
            suggested["tool"].as_str().unwrap(),
            suggested["arguments"].clone(),
        )
        .expect("budget refinement suggested_call must parse");
        assert!(matches!(
            next,
            ToolCall::ReadFiles {
                max_result_bytes: Some(bytes),
                ..
            } if bytes == MAX_SERIALIZED_OUTPUT_BYTES
        ));
    }

    #[test]
    fn output_budget_reserves_space_for_outer_session_metadata() {
        let item = |index, text: String| {
            json!({
                "index": index,
                "path": format!("src/{index}.rs"),
                "success": true,
                "output": {
                    "text": text,
                    "format": "plain",
                    "path": format!("src/{index}.rs"),
                    "sha256": "b".repeat(64),
                    "start_line": 1,
                    "limit": 1,
                    "total_lines": 1,
                    "returned_lines": 1,
                    "end_line": 1,
                    "has_more": false,
                    "next_start_line": null
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
            Some(MAX_SERIALIZED_OUTPUT_BYTES),
            &batch_projection(3, Some(MAX_SERIALIZED_OUTPUT_BYTES)),
        );
        assert_eq!(output["returned_count"], 2);
        assert_eq!(output["next_index"], 2);

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
    fn default_budget_partials_on_line_boundaries_and_explicit_large_returns_more() {
        let lines = (0..900)
            .map(|index| format!("第{index:04}行-{}", "界".repeat(40)))
            .collect::<Vec<_>>();
        let completed = vec![default_complete_item(0, &lines)];

        let default = apply_output_budget(
            "agent:oe:demo",
            1,
            completed.clone(),
            None,
            &batch_projection(1, None),
        );
        let large = apply_output_budget(
            "agent:oe:demo",
            1,
            completed,
            Some(MAX_SERIALIZED_OUTPUT_BYTES),
            &batch_projection(1, Some(MAX_SERIALIZED_OUTPUT_BYTES)),
        );
        let partial = &default["items"][0]["output"];
        let kept = partial["returned_lines"].as_u64().unwrap() as usize;
        assert!(kept > 0 && kept < lines.len());
        assert_eq!(default["next_index"], 0);
        assert_eq!(default["truncation_reason"], "batch_response_budget");
        assert_eq!(partial["budget_truncated"], true);
        assert_eq!(partial["next_start_line"], kept + 1);
        assert_eq!(partial["budget_next_limit"], lines.len() - kept);
        assert_eq!(partial["text"], lines[..kept].join("\n"));
        assert!(std::str::from_utf8(partial["text"].as_str().unwrap().as_bytes()).is_ok());
        assert!(
            serde_json::to_vec(&ToolResult::ok(default)).unwrap().len()
                <= DEFAULT_READ_FILES_RESULT_BYTES
        );
        assert_eq!(large["output_truncated"], false);
        assert_eq!(large["items"][0]["output"]["returned_lines"], lines.len());
    }

    #[test]
    fn read_budget_cursor_reconstructs_original_range_without_gaps() {
        let lines = (0..700)
            .map(|index| format!("line-{index:04}-{}", "x".repeat(90)))
            .collect::<Vec<_>>();
        let first = apply_output_budget(
            "agent:oe:demo",
            1,
            vec![ranged_item(0, 11, &lines)],
            None,
            &batch_projection(1, None),
        );
        let first_output = &first["items"][0]["output"];
        let kept = first_output["returned_lines"].as_u64().unwrap() as usize;
        let next_start = first_output["next_start_line"].as_u64().unwrap() as usize;
        let next_limit = first_output["budget_next_limit"].as_u64().unwrap() as usize;
        assert_eq!(next_start, 11 + kept);
        assert_eq!(next_limit, lines.len() - kept);

        let continuation = apply_output_budget(
            "agent:oe:demo",
            1,
            vec![ranged_item(0, next_start, &lines[kept..])],
            Some(MAX_SERIALIZED_OUTPUT_BYTES),
            &batch_projection(1, Some(MAX_SERIALIZED_OUTPUT_BYTES)),
        );
        let joined = format!(
            "{}\n{}",
            first_output["text"].as_str().unwrap(),
            continuation["items"][0]["output"]["text"].as_str().unwrap()
        );
        assert_eq!(joined, lines.join("\n"));
    }

    #[test]
    fn final_read_trim_preserves_existing_budget_tail_without_gaps() {
        let first_chunk = (0..60)
            .map(|index| format!("line-{index:03}"))
            .collect::<Vec<_>>();
        let mut item = ranged_item(0, 11, &first_chunk);
        item["output"]["limit"] = json!(100);
        item["output"]["total_lines"] = json!(110);
        item["output"]["has_more"] = json!(true);
        item["output"]["next_start_line"] = json!(71);
        item["output"]["budget_truncated"] = json!(true);
        item["output"]["budget_next_limit"] = json!(40);

        let partial = truncate_read_item(&item, 25).expect("second-stage partial read");
        let output = &partial["output"];
        assert_eq!(output["returned_lines"], 25);
        assert_eq!(output["end_line"], 35);
        assert_eq!(output["next_start_line"], 36);
        assert_eq!(output["budget_next_limit"], 75);
        assert_eq!(output["budget_truncated"], true);
    }

    #[test]
    fn read_continuation_byte_measurements_cover_complete_partial_and_batch_recovery() {
        let default_limit =
            webcodex_workspace::file_read_range::EffectiveRange::new(None, None).limit;
        let sha = "e".repeat(64);
        let canonical_bytes = |output: &Value| {
            serde_json::to_vec(&ToolResult::ok(output.clone()))
                .unwrap()
                .len()
        };
        let model_bytes = |tool: &str, output: &Value, projection: &ReadModelProjection| {
            let mut model = ToolResult::ok(output.clone());
            add_actionable_read_continuations(projection, &mut model);
            super::super::dispatch::sparsify_complete_read_success(tool, &mut model);
            serde_json::to_vec(&model).unwrap().len()
        };

        let complete_single = json!({
            "text": "one\ntwo",
            "format": "plain",
            "path": "src/lib.rs",
            "sha256": sha,
            "start_line": 1,
            "limit": default_limit,
            "total_lines": 2,
            "returned_lines": 2,
            "end_line": 2,
            "has_more": false,
            "next_start_line": null
        });
        let single_projection = ReadModelProjection::Single {
            project: "agent:oe:demo".to_string(),
            session_id: None,
            path: "src/lib.rs".to_string(),
            with_line_numbers: None,
        };
        let complete_single_canonical = canonical_bytes(&complete_single);
        let complete_single_model = model_bytes("read_file", &complete_single, &single_projection);

        let partial_single = json!({
            "text": "two",
            "format": "plain",
            "path": "src/lib.rs",
            "sha256": "f".repeat(64),
            "start_line": 2,
            "limit": 1,
            "total_lines": 3,
            "returned_lines": 1,
            "end_line": 2,
            "has_more": true,
            "next_start_line": 3
        });
        let partial_single_canonical = canonical_bytes(&partial_single);
        let partial_single_model = model_bytes("read_file", &partial_single, &single_projection);

        let complete_batch = batch_output(
            "agent:oe:demo",
            2,
            vec![
                default_complete_item(0, &["alpha".to_string()]),
                default_complete_item(1, &["beta".to_string()]),
            ],
            false,
            None,
            None,
        );
        let complete_batch_projection = batch_projection(2, None);
        let complete_batch_canonical = canonical_bytes(&complete_batch);
        let complete_batch_model =
            model_bytes("read_files", &complete_batch, &complete_batch_projection);

        let mut partial_item = ranged_item(0, 11, &["line-11".to_string()]);
        partial_item["output"]["limit"] = json!(1);
        partial_item["output"]["total_lines"] = json!(20);
        partial_item["output"]["has_more"] = json!(true);
        partial_item["output"]["next_start_line"] = json!(12);
        let partial_batch = batch_output("agent:oe:demo", 1, vec![partial_item], false, None, None);
        let partial_batch_projection = ReadModelProjection::Batch {
            project: "agent:oe:demo".to_string(),
            session_id: None,
            items: vec![ReadFilesItem {
                path: "src/0.rs".to_string(),
                start_line: Some(11),
                limit: Some(1),
            }],
            with_line_numbers: None,
            max_result_bytes: None,
        };
        let partial_batch_canonical = canonical_bytes(&partial_batch);
        let partial_batch_model =
            model_bytes("read_files", &partial_batch, &partial_batch_projection);

        let large_projection = batch_projection(2, Some(MAX_SERIALIZED_OUTPUT_BYTES));
        let full_budget_batch = batch_output(
            "agent:oe:demo",
            2,
            vec![
                ranged_item(0, 1, &["x".repeat(140 * 1024)]),
                ranged_item(1, 1, &["y".repeat(140 * 1024)]),
            ],
            false,
            None,
            None,
        );
        let budget_batch_canonical = canonical_bytes(&full_budget_batch);
        let budgeted = apply_output_budget(
            "agent:oe:demo",
            2,
            full_budget_batch["items"].as_array().unwrap().clone(),
            Some(MAX_SERIALIZED_OUTPUT_BYTES),
            &large_projection,
        );
        let budget_batch_model = model_bytes("read_files", &budgeted, &large_projection);

        let partial_lines = (0..900)
            .map(|index| format!("第{index:04}行-{}", "界".repeat(40)))
            .collect::<Vec<_>>();
        let partial_plus_later_full = batch_output(
            "agent:oe:demo",
            3,
            vec![
                default_complete_item(0, &partial_lines),
                ranged_item(1, 1, &["later".to_string()]),
                ranged_item(2, 1, &["last".to_string()]),
            ],
            false,
            None,
            None,
        );
        let partial_plus_later_projection = batch_projection(3, None);
        let partial_plus_later_canonical = canonical_bytes(&partial_plus_later_full);
        let partial_plus_later_budgeted = apply_output_budget(
            "agent:oe:demo",
            3,
            partial_plus_later_full["items"].as_array().unwrap().clone(),
            None,
            &partial_plus_later_projection,
        );
        let partial_plus_later_model = model_bytes(
            "read_files",
            &partial_plus_later_budgeted,
            &partial_plus_later_projection,
        );

        eprintln!(
            "read_continuation_bytes complete_read_file={complete_single_canonical}->{complete_single_model} partial_read_file={partial_single_canonical}->{partial_single_model} complete_read_files={complete_batch_canonical}->{complete_batch_model} partial_item={partial_batch_canonical}->{partial_batch_model} batch_budget={budget_batch_canonical}->{budget_batch_model} partial_plus_batch={partial_plus_later_canonical}->{partial_plus_later_model}"
        );

        assert!(complete_single_model < complete_single_canonical);
        assert!(complete_batch_model < complete_batch_canonical);
        assert!(budget_batch_model <= MAX_SERIALIZED_OUTPUT_BYTES);
        assert!(partial_plus_later_model <= DEFAULT_READ_FILES_RESULT_BYTES);
    }

    #[test]
    fn result_budget_clamps_to_existing_hard_cap() {
        assert_eq!(
            normalized_result_budget(Some(MAX_SERIALIZED_OUTPUT_BYTES * 2)),
            MAX_SERIALIZED_OUTPUT_BYTES
        );
    }
}
