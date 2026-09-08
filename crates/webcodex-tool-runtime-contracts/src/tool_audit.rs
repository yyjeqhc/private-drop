//! Audit-safe argument summaries for runtime tool calls.

#[cfg(test)]
use super::tool_call::ComputerSnapshotRegion;
use super::tool_call::ToolCall;
use super::tool_inputs::{is_checkpoint_kind, is_checkpoint_validation_status};
use serde_json::Value;
use sha2::{Digest, Sha256};
use webcodex_core::audit_preview::{command_preview, process_preview};
use webcodex_core::runner_protocol::{normalize_cargo_value, normalize_rust_test_filter};
use webcodex_workflow_session::SessionExecutionContext;

use webcodex_tool_contracts::{audit_policy::*, lookup_tool_definition};

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuditStage {
    Request,
    Typed,
}

pub fn session_log_arguments_for_tool_request(tool_name: &str, arguments: &Value) -> Value {
    project_arguments(
        lookup_tool_definition(tool_name).map(|definition| &definition.audit),
        arguments,
        AuditStage::Request,
    )
}

fn project_arguments(
    policy: Option<&ToolAuditPolicy>,
    arguments: &Value,
    stage: AuditStage,
) -> Value {
    let Some(policy) = policy.filter(|policy| policy.is_valid()) else {
        return Value::Null;
    };
    let Some(obj) = arguments.as_object() else {
        return Value::Null;
    };
    if stage == AuditStage::Typed && policy.request.typed == AuditTypedPolicy::Omit {
        return serde_json::json!({});
    }
    let mut out = serde_json::Map::new();
    if policy.request.transform != AuditTransform::Fields {
        copy_keys(obj, &mut out, &["project"]);
    }
    apply_fields(policy.request.fields, arguments, &mut out);
    apply_request_transform(policy.request.transform, obj, &mut out, stage);
    if stage == AuditStage::Typed {
        if let AuditTypedPolicy::Overrides { fields, omit } = policy.request.typed {
            apply_fields(fields, arguments, &mut out);
            for key in omit {
                out.remove(*key);
            }
        }
    }
    Value::Object(out)
}

fn source_value<'a>(input: &'a Value, source: &str) -> Option<&'a Value> {
    if source.starts_with('/') {
        input.pointer(source)
    } else {
        input.get(source)
    }
}

fn apply_fields(fields: &[AuditField], input: &Value, out: &mut serde_json::Map<String, Value>) {
    for field in fields {
        let value = source_value(input, field.source);
        let string = value.and_then(Value::as_str);
        let array = value.and_then(Value::as_array);
        let projected = match field.value {
            AuditValue::Copy => {
                let Some(value) = value else {
                    continue;
                };
                value.clone()
            }
            AuditValue::Nullable => value.cloned().unwrap_or(Value::Null),
            AuditValue::KeyPresent => Value::Bool(value.is_some()),
            AuditValue::Present => Value::Bool(value.is_some_and(|value| !value.is_null())),
            AuditValue::StringPresent => Value::Bool(string.is_some()),
            AuditValue::NonemptyString => Value::Bool(string.is_some_and(|s| !s.is_empty())),
            AuditValue::Bytes => Value::from(string.map(str::len).unwrap_or_default()),
            AuditValue::Chars => Value::from(string.map(|s| s.chars().count()).unwrap_or_default()),
            AuditValue::Count => Value::from(array.map(Vec::len).unwrap_or_default()),
            AuditValue::OptionalCount => {
                let Some(array) = array else {
                    continue;
                };
                Value::from(array.len())
            }
            AuditValue::ObjectCount => Value::from(
                value
                    .and_then(Value::as_object)
                    .map(serde_json::Map::len)
                    .unwrap_or_default(),
            ),
            AuditValue::NullableCount => array.map(|a| Value::from(a.len())).unwrap_or(Value::Null),
            AuditValue::NullableBytes => {
                string.map(|s| Value::from(s.len())).unwrap_or(Value::Null)
            }
            AuditValue::Preview => {
                let Some(s) = string else {
                    continue;
                };
                Value::String(command_preview(s))
            }
            AuditValue::ExecutionContext => value
                .cloned()
                .and_then(|v| serde_json::from_value::<SessionExecutionContext>(v).ok())
                .map(|c| c.audit_summary())
                .unwrap_or(Value::Null),
            AuditValue::CompletionFingerprint => bounded_completion_key_fingerprint(string),
            AuditValue::ConsumeTokenPresent => Value::Bool(
                input
                    .get("consume_token_present")
                    .and_then(Value::as_bool)
                    .unwrap_or(string.is_some()),
            ),
            AuditValue::RecipientMode => Value::String(
                if array.is_some() {
                    "explicit"
                } else {
                    "all_agents_except_author"
                }
                .into(),
            ),
            AuditValue::AnyPattern => {
                Value::Bool(array.is_some_and(|a| a.iter().any(|q| q.get("pattern").is_some())))
            }
            AuditValue::ExactCommit => {
                if let Some(obj) = input.as_object() {
                    insert_exact_git_commit_audit(obj, out, field.source);
                }
                continue;
            }
            AuditValue::ValidationIdentity => {
                let Some(identity) = structured_validation_target_identity(field.source, input)
                else {
                    continue;
                };
                Value::String(identity)
            }
        };
        out.insert(field.destination.to_string(), projected);
    }
}

fn apply_request_transform(
    transform: AuditTransform,
    obj: &serde_json::Map<String, Value>,
    out: &mut serde_json::Map<String, Value>,
    stage: AuditStage,
) {
    // serde serializes absent typed Options as null. Raw key-presence summaries
    // deliberately distinguish that from a caller omitting the key entirely.
    let present = |value: Option<&Value>| {
        value.is_some_and(|value| stage == AuditStage::Request || !value.is_null())
    };
    match transform {
        AuditTransform::Fields => {}
        AuditTransform::ProcessExecution => {
            out.insert(
                "executable_present".to_string(),
                Value::Bool(obj.contains_key("executable")),
            );
            out.insert(
                "stdin_present".to_string(),
                Value::Bool(present(obj.get("stdin"))),
            );
            let args = obj.get("args").and_then(Value::as_array);
            out.insert(
                "arg_count".to_string(),
                Value::from(args.map(Vec::len).unwrap_or_default()),
            );
            copy_keys(obj, out, &["timeout_secs", "cwd", "purpose"]);
            if let Some(executable) = obj.get("executable").and_then(Value::as_str) {
                out.insert(
                    "process_summary".to_string(),
                    Value::String(process_preview(
                        executable,
                        args.into_iter().flatten().filter_map(Value::as_str),
                    )),
                );
            }
            if let Some(identity) =
                obj.get("executable")
                    .and_then(Value::as_str)
                    .and_then(|executable| {
                        let args = obj
                            .get("args")
                            .and_then(Value::as_array)
                            .map(|values| {
                                values
                                    .iter()
                                    .map(|value| value.as_str().map(str::to_string))
                                    .collect::<Option<Vec<_>>>()
                            })
                            .flatten()
                            .unwrap_or_default();
                        run_process_validation_identity(
                            executable,
                            &args,
                            obj.get("stdin").and_then(Value::as_str),
                            obj.get("cwd").and_then(Value::as_str),
                            obj.get("purpose").and_then(Value::as_str),
                        )
                    })
            {
                out.insert(
                    "execution_identity".to_string(),
                    Value::String(identity.identity.clone()),
                );
                if is_structured_validation_target_identity(&identity.identity) {
                    out.insert(
                        "validation_target_id".to_string(),
                        Value::String(identity.identity),
                    );
                }
                if let Some(tool) = identity.validation_tool {
                    out.insert(
                        "validation_tool".to_string(),
                        Value::String(tool.to_string()),
                    );
                }
            }
        }
        AuditTransform::DetachedExecution => {
            out.insert(
                "executable_present".to_string(),
                Value::Bool(obj.contains_key("executable")),
            );
            out.insert(
                "stdin_present".to_string(),
                Value::Bool(obj.contains_key("stdin")),
            );
            let arg_count = obj
                .get("args")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or_default();
            out.insert("arg_count".to_string(), Value::from(arg_count));
            out.insert(
                "process_summary".to_string(),
                Value::String(format!("detached process ({arg_count} args)")),
            );
            copy_keys(obj, out, &["timeout_secs", "cwd", "purpose"]);
        }
        AuditTransform::ScriptExecution => {
            if let Some(language) = obj.get("language").cloned() {
                out.insert("language".to_string(), language);
            }
            out.insert(
                "script_bytes".to_string(),
                Value::from(
                    obj.get("script")
                        .and_then(Value::as_str)
                        .map(str::len)
                        .unwrap_or_default(),
                ),
            );
            out.insert(
                "stdin_present".to_string(),
                Value::Bool(obj.get("stdin").is_some_and(|value| !value.is_null())),
            );
            out.insert(
                "arg_count".to_string(),
                Value::from(
                    obj.get("args")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or_default(),
                ),
            );
            copy_keys(obj, out, &["timeout_secs", "cwd", "purpose"]);
            if let (Some(language), Some(script)) = (
                obj.get("language").and_then(Value::as_str),
                obj.get("script").and_then(Value::as_str),
            ) {
                let args = obj
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .map(|value| value.as_str().map(str::to_string))
                            .collect::<Option<Vec<_>>>()
                    })
                    .flatten()
                    .unwrap_or_default();
                if let Some(identity) = run_script_validation_identity(
                    language,
                    script,
                    &args,
                    obj.get("stdin").and_then(Value::as_str),
                    obj.get("cwd").and_then(Value::as_str),
                    obj.get("purpose").and_then(Value::as_str),
                ) {
                    out.insert(
                        "execution_identity".to_string(),
                        Value::String(identity.identity.clone()),
                    );
                    if is_structured_validation_target_identity(&identity.identity) {
                        out.insert(
                            "validation_target_id".to_string(),
                            Value::String(identity.identity),
                        );
                    }
                    if let Some(tool) = identity.validation_tool {
                        out.insert(
                            "validation_tool".to_string(),
                            Value::String(tool.to_string()),
                        );
                    }
                }
            }
        }
        AuditTransform::JobObservation => {
            let items = obj.get("items").and_then(Value::as_array);
            out.insert(
                "item_count".to_string(),
                Value::from(items.map(Vec::len).unwrap_or_default()),
            );
            out.insert(
                "token_count".to_string(),
                Value::from(
                    items
                        .into_iter()
                        .flatten()
                        .filter(|item| present(item.get("after_observation_token")))
                        .count(),
                ),
            );
            out.insert(
                "job_ids".to_string(),
                Value::Array(
                    items
                        .into_iter()
                        .flatten()
                        .filter_map(|item| item.get("job_id").and_then(Value::as_str))
                        .map(|job_id| Value::String(job_id.to_string()))
                        .collect(),
                ),
            );
            copy_keys(obj, out, &["tail_lines", "wait_secs"]);
        }
        AuditTransform::Checkpoint => {
            copy_keys(obj, out, &["title", "include_untracked"]);
            out.insert(
                "note_present".to_string(),
                Value::Bool(present(obj.get("note"))),
            );
            let kind = obj
                .get("kind")
                .and_then(Value::as_str)
                .filter(|value| is_checkpoint_kind(value))
                .unwrap_or(if present(obj.get("kind")) {
                    "invalid"
                } else {
                    "snapshot"
                });
            out.insert("kind".to_string(), Value::String(kind.to_string()));
            let label_count = obj
                .get("labels")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or_default();
            out.insert("label_count".to_string(), Value::from(label_count));
            let validation_status = obj
                .get("validation")
                .and_then(Value::as_object)
                .and_then(|validation| validation.get("status"))
                .and_then(Value::as_str)
                .filter(|value| is_checkpoint_validation_status(value))
                .unwrap_or(
                    if present(
                        obj.get("validation")
                            .and_then(Value::as_object)
                            .and_then(|validation| validation.get("status")),
                    ) {
                        "invalid"
                    } else {
                        "unknown"
                    },
                );
            out.insert(
                "validation_status".to_string(),
                Value::String(validation_status.to_string()),
            );
        }
        AuditTransform::Edits => {
            let changes = obj.get("changes").and_then(Value::as_array);
            out.insert(
                "change_count".into(),
                Value::from(changes.map(Vec::len).unwrap_or_default()),
            );
            for (destination, source) in [
                ("kinds", "kind"),
                ("paths", "path"),
                ("destination_paths", "to_path"),
            ] {
                out.insert(
                    destination.into(),
                    Value::Array(
                        changes
                            .into_iter()
                            .flatten()
                            .filter_map(|change| change.get(source).and_then(Value::as_str))
                            .map(|s| Value::String(s.into()))
                            .collect(),
                    ),
                );
            }
            out.insert(
                "expected_sha256_count".into(),
                Value::from(
                    changes
                        .into_iter()
                        .flatten()
                        .filter(|c| c.get("expected_sha256").is_some_and(|v| !v.is_null()))
                        .count(),
                ),
            );
        }
    }
}
pub fn session_log_result_for_tool(tool_name: &str, output: &Value) -> Value {
    match tool_name {
        "read_tool_trace" => serde_json::json!({
            "trace_ref": output.get("trace_ref").cloned().unwrap_or(Value::Null),
            "trace_mode": output.get("trace_mode").cloned().unwrap_or(Value::Null),
            "payload_count": output.get("payload_count").cloned().unwrap_or(Value::Null),
            "returned_count": output.get("returned_count").cloned().unwrap_or(Value::Null),
            "offset": output.get("offset").cloned().unwrap_or(Value::Null),
            "next_offset": output.get("next_offset").cloned().unwrap_or(Value::Null),
            "payload_index": output.get("payload_index").cloned().unwrap_or(Value::Null),
            "phase": output.get("phase").cloned().unwrap_or(Value::Null),
            "payload_bytes": output.get("payload_bytes").cloned().unwrap_or(Value::Null),
            "payload_sha256": output.get("payload_sha256").cloned().unwrap_or(Value::Null),
            "payload_available": output.get("payload_available").cloned().unwrap_or(Value::Null),
            "reason": output.get("reason").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "coding_agent_start" | "coding_agent_cancel" => serde_json::json!({
            "run_id": output.get("run_id").cloned().unwrap_or(Value::Null),
            "project": output.get("project").cloned().unwrap_or(Value::Null),
            "provider_id": output.get("provider_id").cloned().unwrap_or(Value::Null),
            "state": output.get("state").cloned().unwrap_or(Value::Null),
            "execution_state": output.get("execution_state").cloned().unwrap_or(Value::Null),
            "cancel_requested": output.get("cancel_requested").cloned().unwrap_or(Value::Null),
            "terminal_stop_reason": output.pointer("/terminal/stop_reason").cloned().unwrap_or(Value::Null),
            "terminal_error_code": output.pointer("/terminal/error_code").cloned().unwrap_or(Value::Null),
            "terminal_completed_at": output.pointer("/terminal/completed_at").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "recovery_kind": output.get("recovery_kind").cloned().unwrap_or(Value::Null),
        }),
        "coding_agent_observe" => {
            let mut kind_counts = serde_json::Map::new();
            let mut event_count = 0usize;
            let mut event_body_bytes = 0usize;
            if let Some(events) = output.get("events").and_then(Value::as_array) {
                event_count = events.len();
                for event in events {
                    if let Some(kind) = event.get("kind").and_then(Value::as_str) {
                        let count = kind_counts.get(kind).and_then(Value::as_u64).unwrap_or(0) + 1;
                        kind_counts.insert(kind.to_string(), Value::from(count));
                    }
                    event_body_bytes = event_body_bytes.saturating_add(
                        event
                            .get("text")
                            .and_then(Value::as_str)
                            .map(str::len)
                            .unwrap_or(0),
                    );
                }
            }
            serde_json::json!({
                "run_id": output.get("run_id").cloned().unwrap_or(Value::Null),
                "project": output.get("project").cloned().unwrap_or(Value::Null),
                "provider_id": output.get("provider_id").cloned().unwrap_or(Value::Null),
                "state": output.get("state").cloned().unwrap_or(Value::Null),
                "execution_state": output.get("execution_state").cloned().unwrap_or(Value::Null),
                "event_count": event_count,
                "event_kind_counts": kind_counts,
                "event_body_bytes": event_body_bytes,
                "has_more": output.get("has_more").cloned().unwrap_or(Value::Null),
                "history_lost": output.get("history_lost").cloned().unwrap_or(Value::Null),
                "first_retained_sequence": output.get("first_retained_sequence").cloned().unwrap_or(Value::Null),
                "terminal_stop_reason": output.pointer("/terminal/stop_reason").cloned().unwrap_or(Value::Null),
                "terminal_error_code": output.pointer("/terminal/error_code").cloned().unwrap_or(Value::Null),
                "terminal_completed_at": output.pointer("/terminal/completed_at").cloned().unwrap_or(Value::Null),
                "recovery_kind": output.get("recovery_kind").cloned().unwrap_or(Value::Null),
                "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            })
        }
        "git_review_summary" => serde_json::json!({
            "project": output.get("project").cloned().unwrap_or(Value::Null),
            "scope": output.get("scope").cloned().unwrap_or(Value::Null),
            "stats": output.get("stats").cloned().unwrap_or(Value::Null),
            "coverage": output.get("coverage").cloned().unwrap_or(Value::Null),
            "truncation": output.get("truncation").cloned().unwrap_or(Value::Null),
            "deterministic": output.get("deterministic").cloned().unwrap_or(Value::Null),
            "llm_summary": output.get("llm_summary").cloned().unwrap_or(Value::Null),
            "truncated": output.get("truncated").cloned().unwrap_or(Value::Null),
            "reason_code": output.get("reason_code").cloned().unwrap_or(Value::Null),
            "signal_count": output.get("signals").and_then(Value::as_array).map(Vec::len),
            "file_count": output.get("files").and_then(Value::as_array).map(Vec::len),
        }),
        "git_diff_hunks" => serde_json::json!({
            "project": output.get("project").cloned().unwrap_or(Value::Null),
            "scope": output.get("scope").cloned().unwrap_or(Value::Null),
            "cached": output.get("cached").cloned().unwrap_or(Value::Null),
            "hunk_count": output.get("hunk_count").cloned().unwrap_or(Value::Null),
            "truncated": output.get("truncated").cloned().unwrap_or(Value::Null),
            "truncation_reasons": output.get("truncation_reasons").cloned().unwrap_or(Value::Null),
            "has_more": output.get("has_more").cloned().unwrap_or(Value::Null),
            "exit_code": output.get("exit_code").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "reason_code": output.get("reason_code").cloned().unwrap_or(Value::Null),
            "file_count": output.get("files").and_then(Value::as_array).map(Vec::len),
        }),
        "post_session_message" => serde_json::json!({
            "success": output.get("success").cloned().unwrap_or(Value::Null),
            "session_id": output.get("session_id").cloned().unwrap_or(Value::Null),
            "message_id": output.get("message_id").cloned().unwrap_or(Value::Null),
            "kind": output.pointer("/message/kind").cloned().unwrap_or(Value::Null),
            "status": output.pointer("/message/status").cloned().unwrap_or(Value::Null),
            "requires_ack": output.pointer("/message/requires_ack").cloned().unwrap_or(Value::Null),
            "author_session_id": output.pointer("/message/author_session_id").cloned().unwrap_or(Value::Null),
        }),
        "list_session_messages" => serde_json::json!({
            "success": output.get("success").cloned().unwrap_or(Value::Null),
            "session_id": output.get("session_id").cloned().unwrap_or(Value::Null),
            "message_count": output.get("messages").and_then(Value::as_array).map(Vec::len),
        }),
        "get_session_assignment" => serde_json::json!({
            "success": output.get("success").cloned().unwrap_or(Value::Null),
            "session_id": output.get("session_id").cloned().unwrap_or(Value::Null),
            "message_id": output.get("message_id").cloned().unwrap_or(Value::Null),
            "direct_reply_count": output.get("direct_replies").and_then(Value::as_array).map(Vec::len),
            "assignment_fence_present": output.get("assignment_fence").and_then(Value::as_str).is_some(),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "observe_session_messages" => serde_json::json!({
            "success": output.get("success").cloned().unwrap_or(Value::Null),
            "session_id": output.get("session_id").cloned().unwrap_or(Value::Null),
            "message_count": output.get("messages").and_then(Value::as_array).map(Vec::len),
            "changed": output.get("changed").cloned().unwrap_or(Value::Null),
            "history_lost": output.get("history_lost").cloned().unwrap_or(Value::Null),
            "has_more": output.get("has_more").cloned().unwrap_or(Value::Null),
            "wait_outcome": output.get("wait_outcome").cloned().unwrap_or(Value::Null),
        }),
        "resolve_session_message" => serde_json::json!({
            "success": output.get("success").cloned().unwrap_or(Value::Null),
            "session_id": output.get("session_id").cloned().unwrap_or(Value::Null),
            "message_id": output.get("message_id").cloned().unwrap_or(Value::Null),
            "status": output.pointer("/message/status").cloned().unwrap_or(Value::Null),
            "resolved_by_message_id": output.pointer("/message/resolved_by_message_id").cloned().unwrap_or(Value::Null),
        }),
        "complete_session_message" => serde_json::json!({
            "success": output.get("success").cloned().unwrap_or(Value::Null),
            "session_id": output.get("session_id").cloned().unwrap_or(Value::Null),
            "message_id": output.get("message_id").cloned().unwrap_or(Value::Null),
            "answer_message_id": output.get("answer_message_id").cloned().unwrap_or(Value::Null),
            "completion_id": output.get("completion_id").cloned().unwrap_or(Value::Null),
            "replayed": output.get("replayed").cloned().unwrap_or(Value::Null),
            "author_session_id": output.pointer("/answer/author_session_id").cloned().unwrap_or(Value::Null),
        }),
        "session_discussion_summary" => serde_json::json!({
            "success": output.get("success").cloned().unwrap_or(Value::Null),
            "session_id": output.get("session_id").cloned().unwrap_or(Value::Null),
            "counts": output.get("counts").cloned().unwrap_or(Value::Null),
            "open_todo_count": output.get("open_todos").and_then(Value::as_array).map(Vec::len),
            "recent_answer_count": output.get("recent_answers").and_then(Value::as_array).map(Vec::len),
            "recent_completion_count": output.get("recent_completions").and_then(Value::as_array).map(Vec::len),
        }),
        "session_handoff_summary" => serde_json::json!({
            "session_id": output.get("session_id").cloned().unwrap_or(Value::Null),
            "project": output.get("project").cloned().unwrap_or(Value::Null),
            "lifecycle": output.get("lifecycle").cloned().unwrap_or(Value::Null),
            "counts": output.get("counts").cloned().unwrap_or(Value::Null),
            "open_todo_count": output.get("open_todos").and_then(Value::as_array).map(Vec::len),
            "recent_answer_count": output.get("recent_answers").and_then(Value::as_array).map(Vec::len),
            "recent_completion_count": output.get("recent_completions").and_then(Value::as_array).map(Vec::len),
            "summary_only": output.get("summary_only").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "create_agent_task" | "assign_agent_task" => serde_json::json!({
            "task_id": output.pointer("/task/summary/task_id").cloned().unwrap_or(Value::Null),
            "state": output.pointer("/task/summary/state").cloned().unwrap_or(Value::Null),
            "assignee_agent_id": output.pointer("/task/summary/assignee_agent_id").cloned().unwrap_or(Value::Null),
            "created": output.get("created").cloned().unwrap_or(Value::Null),
            "replayed": output.get("replayed").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "list_agent_tasks" => serde_json::json!({
            "total_count": output.get("total_count").cloned().unwrap_or(Value::Null),
            "returned_count": output.get("tasks").and_then(Value::as_array).map(Vec::len),
            "offset": output.get("offset").cloned().unwrap_or(Value::Null),
            "next_offset": output.get("next_offset").cloned().unwrap_or(Value::Null),
            "truncated": output.get("truncated").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "read_agent_task" => serde_json::json!({
            "task_id": output.pointer("/task/summary/task_id").cloned().unwrap_or(Value::Null),
            "state": output.pointer("/task/summary/state").cloned().unwrap_or(Value::Null),
            "assignee_agent_id": output.pointer("/task/summary/assignee_agent_id").cloned().unwrap_or(Value::Null),
            "latest_attempt_id": output.pointer("/task/summary/latest_attempt/attempt_id").cloned().unwrap_or(Value::Null),
            "latest_attempt_state": output.pointer("/task/summary/latest_attempt/state").cloned().unwrap_or(Value::Null),
            "latest_attempt_number": output.pointer("/task/summary/latest_attempt/attempt_number").cloned().unwrap_or(Value::Null),
            "controller_generation": output.pointer("/task/summary/latest_attempt/attempt_controller_generation").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "start_agent_task_attempt" => serde_json::json!({
            "task_id": output.pointer("/task/task_id").cloned().unwrap_or(Value::Null),
            "task_state": output.pointer("/task/state").cloned().unwrap_or(Value::Null),
            "attempt_id": output.pointer("/attempt/attempt_id").cloned().unwrap_or(Value::Null),
            "attempt_number": output.pointer("/attempt/attempt_number").cloned().unwrap_or(Value::Null),
            "attempt_state": output.pointer("/attempt/state").cloned().unwrap_or(Value::Null),
            "controller_generation": output.pointer("/attempt/attempt_controller_generation").cloned().unwrap_or(Value::Null),
            "replayed": output.get("replayed").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "start_agent_task_coding_run" | "reconcile_agent_task_coding_run" => serde_json::json!({
            "task_id": output.get("task_id").cloned().unwrap_or(Value::Null),
            "attempt_id": output.get("attempt_id").cloned().unwrap_or(Value::Null),
            "run_id": output.get("run_id").cloned().unwrap_or(Value::Null),
            "project": output.get("project").cloned().unwrap_or(Value::Null),
            "provider_id": output.get("provider_id").cloned().unwrap_or(Value::Null),
            "dispatch_state": output.get("dispatch_state").cloned().unwrap_or(Value::Null),
            "run_state": output.get("run_state").cloned().unwrap_or(Value::Null),
            "execution_state": output.get("execution_state").cloned().unwrap_or(Value::Null),
            "execution_status": output.get("execution_status").cloned().unwrap_or(Value::Null),
            "execution_recovery": output.get("execution_recovery").cloned().unwrap_or(Value::Null),
            "task_state": output.get("task_state").cloned().unwrap_or(Value::Null),
            "attempt_state": output.get("attempt_state").cloned().unwrap_or(Value::Null),
            "replayed": output.get("replayed").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "heartbeat_agent_task_attempt" | "complete_agent_task_attempt" => serde_json::json!({
            "task_id": output.pointer("/task/task_id").cloned().unwrap_or(Value::Null),
            "task_state": output.pointer("/task/state").cloned().unwrap_or(Value::Null),
            "attempt_id": output.pointer("/attempt/attempt_id").cloned().unwrap_or(Value::Null),
            "attempt_number": output.pointer("/attempt/attempt_number").cloned().unwrap_or(Value::Null),
            "attempt_state": output.pointer("/attempt/state").cloned().unwrap_or(Value::Null),
            "controller_generation": output.pointer("/attempt/attempt_controller_generation").cloned().unwrap_or(Value::Null),
            "replayed": output.get("replayed").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "create_agent_identity" | "update_agent_identity" => serde_json::json!({
            "agent_id": output.pointer("/agent/agent_id").cloned().unwrap_or(Value::Null),
            "profile_revision": output.pointer("/agent/profile_revision").cloned().unwrap_or(Value::Null),
            "created": output.get("created").cloned().unwrap_or(Value::Null),
            "replayed": output.get("replayed").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "list_agent_identities" => serde_json::json!({
            "total_count": output.get("total_count").cloned().unwrap_or(Value::Null),
            "returned_count": output.get("agents").and_then(Value::as_array).map(Vec::len),
            "offset": output.get("offset").cloned().unwrap_or(Value::Null),
            "next_offset": output.get("next_offset").cloned().unwrap_or(Value::Null),
            "truncated": output.get("truncated").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "attach_agent_endpoint" | "detach_agent_endpoint" => serde_json::json!({
            "endpoint_id": output.pointer("/endpoint/endpoint_id").cloned().unwrap_or(Value::Null),
            "agent_id": output.pointer("/endpoint/agent_id").cloned().unwrap_or(Value::Null),
            "detached": output.pointer("/endpoint/detached_at_unix_ms").is_some_and(|value| !value.is_null()),
            "created": output.get("created").cloned().unwrap_or(Value::Null),
            "replayed": output.get("replayed").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "create_conversation" => serde_json::json!({
            "conversation_id": output.pointer("/conversation/conversation/conversation_id").cloned().unwrap_or(Value::Null),
            "participant_count": output.pointer("/conversation/participants").and_then(Value::as_array).map(Vec::len),
            "message_count": output.pointer("/conversation/messages").and_then(Value::as_array).map(Vec::len),
            "created": output.get("created").cloned().unwrap_or(Value::Null),
            "replayed": output.get("replayed").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "list_conversations" => serde_json::json!({
            "total_count": output.get("total_count").cloned().unwrap_or(Value::Null),
            "returned_count": output.get("conversations").and_then(Value::as_array).map(Vec::len),
            "offset": output.get("offset").cloned().unwrap_or(Value::Null),
            "next_offset": output.get("next_offset").cloned().unwrap_or(Value::Null),
            "truncated": output.get("truncated").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "read_conversation" => serde_json::json!({
            "conversation_id": output.pointer("/conversation/conversation_id").cloned().unwrap_or(Value::Null),
            "participant_count": output.get("participants").and_then(Value::as_array).map(Vec::len),
            "message_count": output.get("messages").and_then(Value::as_array).map(Vec::len),
            "after_seq": output.get("after_seq").cloned().unwrap_or(Value::Null),
            "next_after_seq": output.get("next_after_seq").cloned().unwrap_or(Value::Null),
            "truncated": output.get("truncated").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "post_conversation_message" => serde_json::json!({
            "message_id": output.pointer("/message/message_id").cloned().unwrap_or(Value::Null),
            "conversation_id": output.pointer("/message/conversation_id").cloned().unwrap_or(Value::Null),
            "seq": output.pointer("/message/seq").cloned().unwrap_or(Value::Null),
            "delivery_count": output.pointer("/message/deliveries").and_then(Value::as_array).map(Vec::len),
            "replayed": output.get("replayed").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "list_agent_inbox" => serde_json::json!({
            "agent_id": output.get("agent_id").cloned().unwrap_or(Value::Null),
            "total_queued_count": output.get("total_queued_count").cloned().unwrap_or(Value::Null),
            "returned_count": output.get("deliveries").and_then(Value::as_array).map(Vec::len),
            "after_delivery_order": output.get("after_delivery_order").cloned().unwrap_or(Value::Null),
            "next_after_delivery_order": output.get("next_after_delivery_order").cloned().unwrap_or(Value::Null),
            "truncated": output.get("truncated").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "consume_agent_deliveries" => serde_json::json!({
            "agent_id": output.get("agent_id").cloned().unwrap_or(Value::Null),
            "consumed_count": output.get("consumed_delivery_ids").and_then(Value::as_array).map(Vec::len),
            "already_consumed_count": output.get("already_consumed_delivery_ids").and_then(Value::as_array).map(Vec::len),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "bootstrap_agent_conversation" => serde_json::json!({
            "agent_id": output.pointer("/acting_agent/agent_id").cloned().unwrap_or(Value::Null),
            "endpoint_id": output.pointer("/endpoint/endpoint_id").cloned().unwrap_or(Value::Null),
            "controller_generation": output.pointer("/endpoint/controller_generation").cloned().unwrap_or(Value::Null),
            "conversation_id": output.pointer("/selected_conversation/conversation_id").cloned().unwrap_or(Value::Null),
            "queued_delivery_count": output.pointer("/inbox/queued_delivery_count").cloned().unwrap_or(Value::Null),
            "wake_id": output.pointer("/wake/wake_id").cloned().unwrap_or(Value::Null),
            "wake_state": output.pointer("/wake/state").cloned().unwrap_or(Value::Null),
            "adapter_kind": output.pointer("/host_binding/adapter_kind").cloned().unwrap_or(Value::Null),
            "runtime_wake_capable": output.pointer("/host_binding/runtime_wake_capable").cloned().unwrap_or(Value::Null),
            "production_auto_resume_available": output.pointer("/host_binding/production_auto_resume_available").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "consume_agent_wake" => serde_json::json!({
            "wake_id": output.get("wake_id").cloned().unwrap_or(Value::Null),
            "target_agent_id": output.get("target_agent_id").cloned().unwrap_or(Value::Null),
            "state": output.get("state").cloned().unwrap_or(Value::Null),
            "already_consumed": output.get("already_consumed").cloned().unwrap_or(Value::Null),
            "consumed_at_unix_ms": output.get("consumed_at_unix_ms").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
        "memory_search" => serde_json::json!({
            "project": output.get("project").cloned().unwrap_or(Value::Null),
            "catalog_revision": output.get("catalog_revision").cloned().unwrap_or(Value::Null),
            "total_count": output.get("total_count").cloned().unwrap_or(Value::Null),
            "returned_count": output.get("returned_count").cloned().unwrap_or(Value::Null),
            "truncated": output.get("truncated").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
        }),
        "memory_read" => serde_json::json!({
            "project": output.get("project").cloned().unwrap_or(Value::Null),
            "memory_id": output.get("memory_id").cloned().unwrap_or(Value::Null),
            "memory_key": output.get("memory_key").cloned().unwrap_or(Value::Null),
            "revision": output.get("revision").cloned().unwrap_or(Value::Null),
            "bootstrap": output.get("bootstrap").cloned().unwrap_or(Value::Null),
            "priority": output.get("priority").cloned().unwrap_or(Value::Null),
            "returned_body_bytes": output.get("body").and_then(Value::as_str).map(str::len),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
        }),
        "memory_set" => serde_json::json!({
            "project": output.get("project").cloned().unwrap_or(Value::Null),
            "memory_id": output.get("memory_id").cloned().unwrap_or(Value::Null),
            "memory_key": output.get("memory_key").cloned().unwrap_or(Value::Null),
            "old_revision": output.get("old_revision").cloned().unwrap_or(Value::Null),
            "revision": output.get("revision").cloned().unwrap_or(Value::Null),
            "created": output.get("created").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
        }),
        "memory_delete" => serde_json::json!({
            "project": output.get("project").cloned().unwrap_or(Value::Null),
            "memory_id": output.get("memory_id").cloned().unwrap_or(Value::Null),
            "memory_key": output.get("memory_key").cloned().unwrap_or(Value::Null),
            "revision": output.get("revision").cloned().unwrap_or(Value::Null),
            "deleted": output.get("deleted").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
        }),
        "memory_scope_list" => serde_json::json!({
            "total_count": output.get("total_count").cloned().unwrap_or(Value::Null),
            "returned_count": output.get("returned_count").cloned().unwrap_or(Value::Null),
            "truncated": output.get("truncated").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
        }),
        "memory_scope_purge" => serde_json::json!({
            "memory_scope_id": output.get("memory_scope_id").cloned().unwrap_or(Value::Null),
            "catalog_revision": output.get("catalog_revision").cloned().unwrap_or(Value::Null),
            "current_catalog_revision": output.get("current_catalog_revision").cloned().unwrap_or(Value::Null),
            "purged_count": output.get("purged_count").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
        }),
        "skill_list" => serde_json::json!({
            "project": output.get("project").cloned().unwrap_or(Value::Null),
            "catalog_revision": output.get("catalog_revision").cloned().unwrap_or(Value::Null),
            "total_count": output.get("total_count").cloned().unwrap_or(Value::Null),
            "returned_count": output.get("returned_count").cloned().unwrap_or(Value::Null),
            "truncated": output.get("truncated").cloned().unwrap_or(Value::Null),
            "invalid_count": output.get("invalid_count").cloned().unwrap_or(Value::Null),
            "discovery_truncated": output.get("discovery_truncated").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
        }),
        "skill_read_file" => serde_json::json!({
            "project": output.get("project").cloned().unwrap_or(Value::Null),
            "skill_id": output.get("skill_id").cloned().unwrap_or(Value::Null),
            "source_scope": output.get("source_scope").cloned().unwrap_or(Value::Null),
            "trust": output.get("trust").cloned().unwrap_or(Value::Null),
            "package_revision": output.get("package_revision").cloned().unwrap_or(Value::Null),
            "definition_revision": output.get("definition_revision").cloned().unwrap_or(Value::Null),
            "path": output.get("path").cloned().unwrap_or(Value::Null),
            "sha256": output.get("sha256").cloned().unwrap_or(Value::Null),
            "start_line": output.get("start_line").cloned().unwrap_or(Value::Null),
            "end_line": output.get("end_line").cloned().unwrap_or(Value::Null),
            "returned_lines": output.get("returned_lines").cloned().unwrap_or(Value::Null),
            "has_more": output.get("has_more").cloned().unwrap_or(Value::Null),
            "next_start_line": output.get("next_start_line").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
        }),
        "skill_versions" => serde_json::json!({
            "project": output.get("project").cloned().unwrap_or(Value::Null),
            "skill_id": output.get("skill_id").cloned().unwrap_or(Value::Null),
            "skill_key": output.get("skill_key").cloned().unwrap_or(Value::Null),
            "state_revision": output.get("state_revision").cloned().unwrap_or(Value::Null),
            "active_package_revision": output.get("active_package_revision").cloned().unwrap_or(Value::Null),
            "total_count": output.get("total_count").cloned().unwrap_or(Value::Null),
            "offset": output.get("offset").cloned().unwrap_or(Value::Null),
            "next_offset": output.get("next_offset").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
        }),
        "skill_install" => serde_json::json!({
            "project": output.get("project").cloned().unwrap_or(Value::Null),
            "skill_id": output.get("skill_id").cloned().unwrap_or(Value::Null),
            "skill_key": output.get("skill_key").cloned().unwrap_or(Value::Null),
            "package_revision": output.get("package_revision").cloned().unwrap_or(Value::Null),
            "definition_revision": output.get("definition_revision").cloned().unwrap_or(Value::Null),
            "artifact_sha256": output.get("artifact_sha256").cloned().unwrap_or(Value::Null),
            "file_count": output.get("file_count").cloned().unwrap_or(Value::Null),
            "total_bytes": output.get("total_bytes").cloned().unwrap_or(Value::Null),
            "installed": output.get("installed").cloned().unwrap_or(Value::Null),
            "activated": output.get("activated").cloned().unwrap_or(Value::Null),
            "replayed": output.get("replayed").cloned().unwrap_or(Value::Null),
            "state_revision": output.get("state_revision").cloned().unwrap_or(Value::Null),
            "active_package_revision": output.get("active_package_revision").cloned().unwrap_or(Value::Null),
            "outcome_unknown": output.get("outcome_unknown").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
        }),
        "skill_activate" => serde_json::json!({
            "project": output.get("project").cloned().unwrap_or(Value::Null),
            "skill_id": output.get("skill_id").cloned().unwrap_or(Value::Null),
            "skill_key": output.get("skill_key").cloned().unwrap_or(Value::Null),
            "previous_active_package_revision": output.get("previous_active_package_revision").cloned().unwrap_or(Value::Null),
            "active_package_revision": output.get("active_package_revision").cloned().unwrap_or(Value::Null),
            "state_revision": output.get("state_revision").cloned().unwrap_or(Value::Null),
            "changed": output.get("changed").cloned().unwrap_or(Value::Null),
            "replayed": output.get("replayed").cloned().unwrap_or(Value::Null),
            "outcome_unknown": output.get("outcome_unknown").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
        }),
        "skill_remove_revision" => serde_json::json!({
            "project": output.get("project").cloned().unwrap_or(Value::Null),
            "skill_id": output.get("skill_id").cloned().unwrap_or(Value::Null),
            "skill_key": output.get("skill_key").cloned().unwrap_or(Value::Null),
            "package_revision": output.get("package_revision").cloned().unwrap_or(Value::Null),
            "state_revision": output.get("state_revision").cloned().unwrap_or(Value::Null),
            "removed": output.get("removed").cloned().unwrap_or(Value::Null),
            "replayed": output.get("replayed").cloned().unwrap_or(Value::Null),
            "outcome_unknown": output.get("outcome_unknown").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
        }),
        "computer_list_targets" => serde_json::json!({
            "count": output.get("count").cloned().unwrap_or(Value::Null),
            "total_count": output.get("total_count").cloned().unwrap_or(Value::Null),
            "truncated": output.get("truncated").cloned().unwrap_or(Value::Null),
        }),
        "computer_list_windows" => serde_json::json!({
            "count": output.get("count").cloned().unwrap_or(Value::Null),
            "truncated": output.get("truncated").cloned().unwrap_or(Value::Null),
        }),
        "computer_list_applications" => serde_json::json!({
            "count": output.get("count").cloned().unwrap_or(Value::Null),
            "truncated": output.get("truncated").cloned().unwrap_or(Value::Null),
        }),
        "computer_list_displays" => serde_json::json!({
            "count": output.get("count").cloned().unwrap_or(Value::Null),
            "truncated": output.get("truncated").cloned().unwrap_or(Value::Null),
        }),
        "computer_launch_application" => serde_json::json!({
            "application_id": output.get("application_id").cloned().unwrap_or(Value::Null),
            "success": output.get("success").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "execution_state": output.get("execution_state").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
        }),
        "computer_pointer_move" | "computer_pointer_click" => serde_json::json!({
            "display_id": output.get("display_id").cloned().unwrap_or(Value::Null),
            "snapshot_generation": output.get("snapshot_generation").cloned().unwrap_or(Value::Null),
            "x": output.get("x").cloned().unwrap_or(Value::Null),
            "y": output.get("y").cloned().unwrap_or(Value::Null),
            "success": output.get("success").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "execution_state": output.get("execution_state").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
        }),
        "computer_read_clipboard" => serde_json::json!({
            "available": output.get("available").cloned().unwrap_or(Value::Null),
            "text_bytes": output.get("text_bytes").cloned().unwrap_or(Value::Null),
            "success": output.get("success").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "execution_state": output.get("execution_state").cloned().unwrap_or(Value::Null),
        }),
        "computer_write_clipboard" => serde_json::json!({
            "text_bytes": output.get("text_bytes").cloned().unwrap_or(Value::Null),
            "success": output.get("success").cloned().unwrap_or(Value::Null),
            "error_kind": output.get("error_kind").cloned().unwrap_or(Value::Null),
            "execution_state": output.get("execution_state").cloned().unwrap_or(Value::Null),
            "state_changed": output.get("state_changed").cloned().unwrap_or(Value::Null),
        }),
        "computer_snapshot" => serde_json::json!({
            "surface_id": output.pointer("/surface/surface_id").cloned().unwrap_or(Value::Null),
            "source_width": output.get("source_width").cloned().unwrap_or(Value::Null),
            "source_height": output.get("source_height").cloned().unwrap_or(Value::Null),
            "region_present": output.get("region").is_some(),
            "width": output.get("width").cloned().unwrap_or(Value::Null),
            "height": output.get("height").cloned().unwrap_or(Value::Null),
            "mime_type": output.get("mime_type").cloned().unwrap_or(Value::Null),
            "file_bytes": output.get("file_bytes").cloned().unwrap_or(Value::Null),
            "captured_at_unix_ms": output.get("captured_at_unix_ms").cloned().unwrap_or(Value::Null),
        }),
        "computer_snapshot_display" => serde_json::json!({
            "display_id": output.get("display_id").cloned().unwrap_or(Value::Null),
            "snapshot_generation": output.get("snapshot_generation").cloned().unwrap_or(Value::Null),
            "source_width": output.get("source_width").cloned().unwrap_or(Value::Null),
            "source_height": output.get("source_height").cloned().unwrap_or(Value::Null),
            "width": output.get("width").cloned().unwrap_or(Value::Null),
            "height": output.get("height").cloned().unwrap_or(Value::Null),
            "mime_type": output.get("mime_type").cloned().unwrap_or(Value::Null),
            "file_bytes": output.get("file_bytes").cloned().unwrap_or(Value::Null),
            "sha256": output.get("sha256").cloned().unwrap_or(Value::Null),
            "captured_at_unix_ms": output.get("captured_at_unix_ms").cloned().unwrap_or(Value::Null),
        }),
        "computer_save_snapshot" => serde_json::json!({
            "project": output.get("project").cloned().unwrap_or(Value::Null),
            "path": output.get("path").cloned().unwrap_or(Value::Null),
            "client_id": output.get("client_id").cloned().unwrap_or(Value::Null),
            "surface_id": output.get("surface_id").cloned().unwrap_or(Value::Null),
            "source_width": output.get("source_width").cloned().unwrap_or(Value::Null),
            "source_height": output.get("source_height").cloned().unwrap_or(Value::Null),
            "region_present": output.get("region").is_some(),
            "width": output.get("width").cloned().unwrap_or(Value::Null),
            "height": output.get("height").cloned().unwrap_or(Value::Null),
            "mime_type": output.get("mime_type").cloned().unwrap_or(Value::Null),
            "file_bytes": output.get("file_bytes").cloned().unwrap_or(Value::Null),
            "saved": output.get("saved").cloned().unwrap_or(Value::Null),
        }),
        "computer_accessibility_status" => serde_json::json!({
            "platform": output.get("platform").cloned().unwrap_or(Value::Null),
            "trusted": output.get("trusted").cloned().unwrap_or(Value::Null),
        }),
        "computer_accessibility_tree" => serde_json::json!({
            "surface_id": output.get("surface_id").cloned().unwrap_or(Value::Null),
            "observation_generation": output.get("observation_generation").cloned().unwrap_or(Value::Null),
            "node_count": output.get("node_count").cloned().unwrap_or(Value::Null),
            "truncated": output.get("truncated").cloned().unwrap_or(Value::Null),
            "max_depth": output.get("max_depth").cloned().unwrap_or(Value::Null),
            "max_nodes": output.get("max_nodes").cloned().unwrap_or(Value::Null),
        }),
        "computer_find_elements" => serde_json::json!({
            "surface_id": output.get("surface_id").cloned().unwrap_or(Value::Null),
            "observation_generation": output.get("observation_generation").cloned().unwrap_or(Value::Null),
            "count": output.get("count").cloned().unwrap_or(Value::Null),
            "scanned_nodes": output.get("scanned_nodes").cloned().unwrap_or(Value::Null),
            "truncated": output.get("truncated").cloned().unwrap_or(Value::Null),
        }),
        "computer_element_state" => serde_json::json!({
            "surface_id": output.get("surface_id").cloned().unwrap_or(Value::Null),
            "element_id": output.get("element_id").cloned().unwrap_or(Value::Null),
            "observation_generation": output.get("observation_generation").cloned().unwrap_or(Value::Null),
        }),
        "computer_activate_window" => serde_json::json!({
            "surface_id": output.get("surface_id").cloned().unwrap_or(Value::Null),
            "success": output.get("success").cloned().unwrap_or(Value::Null),
        }),
        "computer_control" => serde_json::json!({
            "surface_id": output.get("surface_id").cloned().unwrap_or(Value::Null),
            "element_id": output.get("element_id").cloned().unwrap_or(Value::Null),
            "action": output.get("action").cloned().unwrap_or(Value::Null),
            "success": output.get("success").cloned().unwrap_or(Value::Null),
        }),
        "computer_scroll_to_element" => serde_json::json!({
            "surface_id": output.get("surface_id").cloned().unwrap_or(Value::Null),
            "element_id": output.get("element_id").cloned().unwrap_or(Value::Null),
            "success": output.get("success").cloned().unwrap_or(Value::Null),
        }),
        "computer_key_input" => serde_json::json!({
            "surface_id": output.get("surface_id").cloned().unwrap_or(Value::Null),
            "key": output.get("key").cloned().unwrap_or(Value::Null),
            "modifiers": output.get("modifiers").cloned().unwrap_or(Value::Null),
            "success": output.get("success").cloned().unwrap_or(Value::Null),
        }),
        "computer_input_text" => serde_json::json!({
            "surface_id": output.get("surface_id").cloned().unwrap_or(Value::Null),
            "element_id": output.get("element_id").cloned().unwrap_or(Value::Null),
            "text_bytes": output.get("text_bytes").cloned().unwrap_or(Value::Null),
            "success": output.get("success").cloned().unwrap_or(Value::Null),
        }),
        _ => output.clone(),
    }
}

fn bounded_completion_key_fingerprint(value: Option<&str>) -> Value {
    let Some(value) = value else {
        return Value::Null;
    };
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 128 {
        return Value::String("invalid".to_string());
    }
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex.session-message-completion.v1\0");
    hasher.update(value.as_bytes());
    Value::String(format!("{:x}", hasher.finalize()))
}

fn copy_keys(
    obj: &serde_json::Map<String, Value>,
    out: &mut serde_json::Map<String, Value>,
    keys: &[&str],
) {
    for key in keys {
        if let Some(value) = obj.get(*key).cloned() {
            out.insert((*key).to_string(), value);
        }
    }
}

fn normalized_exact_git_commit_for_audit(value: &str) -> Option<String> {
    (value.len() == 40 && value.as_bytes().iter().all(u8::is_ascii_hexdigit))
        .then(|| value.to_ascii_lowercase())
}

fn insert_exact_git_commit_audit(
    obj: &serde_json::Map<String, Value>,
    out: &mut serde_json::Map<String, Value>,
    key: &str,
) {
    let normalized = obj
        .get(key)
        .and_then(Value::as_str)
        .and_then(normalized_exact_git_commit_for_audit);
    out.insert(format!("{key}_valid"), Value::Bool(normalized.is_some()));
    if let Some(normalized) = normalized {
        out.insert(key.to_string(), Value::String(normalized));
    }
}

pub use webcodex_core::validation_identity::{
    assertion_validation_identity, is_structured_validation_target_identity,
    is_validation_execution_identity, structured_validation_target_identity,
};
use webcodex_core::validation_identity::{
    GENERIC_VALIDATION_IDENTITY_PREFIX, VALIDATION_IDENTITY_HEX_LEN,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenericValidationIdentity {
    pub identity: String,
    pub validation_tool: Option<&'static str>,
}

fn validation_like_purpose(purpose: Option<&str>) -> bool {
    purpose.is_some_and(|purpose| {
        matches!(
            purpose,
            "validation" | "test" | "build" | "format" | "release"
        )
    })
}

fn generic_validation_digest<'a>(
    source: &str,
    purpose: &str,
    cwd: Option<&str>,
    parts: impl IntoIterator<Item = &'a str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex-generic-validation-v1\0");
    hasher.update(source.as_bytes());
    hasher.update(b"\0");
    hasher.update(purpose.as_bytes());
    hasher.update(b"\0");
    hasher.update(cwd.unwrap_or(".").as_bytes());
    for part in parts {
        hasher.update(b"\0");
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = format!("{:x}", hasher.finalize());
    format!(
        "{GENERIC_VALIDATION_IDENTITY_PREFIX}{}",
        &digest[..VALIDATION_IDENTITY_HEX_LEN]
    )
}

fn canonical_cargo_validation_target(
    argv: &[String],
    cwd: Option<&str>,
) -> Option<(&'static str, String)> {
    let (subcommand, rest) = argv.split_first()?;
    let mut input = serde_json::Map::new();
    input.insert(
        "cwd".to_string(),
        Value::String(cwd.unwrap_or(".").to_string()),
    );
    let tool = match subcommand.as_str() {
        "fmt" => {
            let check = match rest {
                [] => false,
                [separator, check] if separator == "--" && check == "--check" => true,
                _ => return None,
            };
            input.insert("check".to_string(), Value::Bool(check));
            "cargo_fmt"
        }
        "check" | "test" => {
            let is_test = subcommand == "test";
            let mut package: Option<String> = None;
            let mut features: Option<String> = None;
            let mut filter: Option<String> = None;
            let mut all_targets = false;
            let mut all_features = false;
            let mut no_default_features = false;
            let mut no_run = false;
            let mut index = 0;
            while index < rest.len() {
                let arg = &rest[index];
                match arg.as_str() {
                    "-p" | "--package" | "--features" => {
                        let value = rest.get(index + 1)?.clone();
                        let slot = if arg == "--features" {
                            &mut features
                        } else {
                            &mut package
                        };
                        if slot.replace(value).is_some() {
                            return None;
                        }
                        index += 2;
                        continue;
                    }
                    "--all-targets" if !all_targets => all_targets = true,
                    "--all-features" if !all_features => all_features = true,
                    "--no-default-features" if !no_default_features => no_default_features = true,
                    "--no-run" if is_test && !no_run => no_run = true,
                    _ if arg.starts_with("--package=") && package.is_none() => {
                        package = Some(arg.trim_start_matches("--package=").to_string());
                    }
                    _ if arg.starts_with("--features=") && features.is_none() => {
                        features = Some(arg.trim_start_matches("--features=").to_string());
                    }
                    _ if is_test && !arg.starts_with('-') && filter.is_none() => {
                        filter = Some(arg.to_string());
                    }
                    _ => return None,
                }
                index += 1;
            }
            let package = match package {
                Some(value) => normalize_cargo_value(&value).ok()?,
                None => None,
            };
            let features = match features {
                Some(value) => normalize_cargo_value(&value).ok()?,
                None => None,
            };
            input.insert("package".to_string(), serde_json::json!(package));
            input.insert("features".to_string(), serde_json::json!(features));
            input.insert("all_targets".to_string(), Value::Bool(all_targets));
            input.insert("all_features".to_string(), Value::Bool(all_features));
            input.insert(
                "no_default_features".to_string(),
                Value::Bool(no_default_features),
            );
            if is_test {
                let filter = match filter {
                    Some(value) => normalize_rust_test_filter(&value).ok()?,
                    None => None,
                };
                input.insert("filter".to_string(), serde_json::json!(filter));
                input.insert("no_run".to_string(), Value::Bool(no_run));
                "cargo_test"
            } else {
                "cargo_check"
            }
        }
        _ => return None,
    };
    let identity = structured_validation_target_identity(tool, &Value::Object(input))?;
    Some((tool, identity))
}

pub fn run_process_validation_identity(
    executable: &str,
    args: &[String],
    stdin: Option<&str>,
    cwd: Option<&str>,
    purpose: Option<&str>,
) -> Option<GenericValidationIdentity> {
    if !validation_like_purpose(purpose) {
        return None;
    }
    if executable == "cargo" && stdin.is_none() {
        if let Some((validation_tool, identity)) = canonical_cargo_validation_target(args, cwd) {
            return Some(GenericValidationIdentity {
                identity,
                validation_tool: Some(validation_tool),
            });
        }
    }
    let purpose = purpose?;
    let mut parts = Vec::with_capacity(args.len() + 2);
    parts.push(executable);
    parts.extend(args.iter().map(String::as_str));
    if let Some(stdin) = stdin {
        parts.push(stdin);
    }
    Some(GenericValidationIdentity {
        identity: generic_validation_digest("run_process", purpose, cwd, parts),
        validation_tool: None,
    })
}

fn simple_script_argv(script: &str) -> Option<Vec<String>> {
    let trimmed = script.trim();
    if trimmed.is_empty()
        || trimmed.lines().count() != 1
        || trimmed.chars().any(|character| {
            matches!(
                character,
                ';' | '|' | '&' | '$' | '`' | '\\' | '\'' | '"' | '<' | '>' | '(' | ')' | '{' | '}'
            )
        })
    {
        return None;
    }
    let argv = trimmed
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    (!argv.is_empty()).then_some(argv)
}

pub fn run_script_validation_identity(
    language: &str,
    script: &str,
    args: &[String],
    stdin: Option<&str>,
    cwd: Option<&str>,
    purpose: Option<&str>,
) -> Option<GenericValidationIdentity> {
    if !validation_like_purpose(purpose) {
        return None;
    }
    if matches!(language, "sh" | "bash") && args.is_empty() && stdin.is_none() {
        if let Some(argv) = simple_script_argv(script) {
            if argv.first().is_some_and(|program| program == "cargo") {
                if let Some((validation_tool, identity)) =
                    canonical_cargo_validation_target(&argv[1..], cwd)
                {
                    return Some(GenericValidationIdentity {
                        identity,
                        validation_tool: Some(validation_tool),
                    });
                }
            }
        }
    }
    let purpose = purpose?;
    let mut parts = Vec::with_capacity(args.len() + 3);
    parts.push(language);
    parts.push(script);
    parts.extend(args.iter().map(String::as_str));
    if let Some(stdin) = stdin {
        parts.push(stdin);
    }
    Some(GenericValidationIdentity {
        identity: generic_validation_digest("run_script", purpose, cwd, parts),
        validation_tool: None,
    })
}

#[cfg(test)]
mod computer_privacy_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn computer_application_list_ledger_omits_names_ids_and_native_identity() {
        let output = json!({
            "applications": [{
                "application_id": "application_0123456789abcdef0123456789abcdef",
                "display_name": "Private App",
                "native_identity": "never-allowed"
            }],
            "count": 1,
            "truncated": false
        });
        let summary = session_log_result_for_tool("computer_list_applications", &output);
        let serialized = serde_json::to_string(&summary).unwrap();
        assert_eq!(summary, json!({"count": 1, "truncated": false}));
        assert!(!serialized.contains("Private App"));
        assert!(!serialized.contains("application_"));
        assert!(!serialized.contains("native_identity"));
    }

    #[test]
    fn trace_reader_audit_result_never_persists_raw_payload() {
        let summary = session_log_result_for_tool(
            "read_tool_trace",
            &json!({
                "trace_ref": "01234567-89ab-cdef-0123-456789abcdef",
                "trace_mode": "full",
                "payload_index": 2,
                "phase": "final_response",
                "payload_bytes": 123,
                "payload_sha256": "a".repeat(64),
                "payload_available": true,
                "payload": {
                    "private_token": "PRIVATE_RAW_TRACE_BODY",
                    "stdout": "PRIVATE_OUTPUT"
                }
            }),
        );
        let serialized = serde_json::to_string(&summary).unwrap();
        assert_eq!(summary["payload_index"], 2);
        assert_eq!(summary["phase"], "final_response");
        assert_eq!(summary["payload_bytes"], 123);
        assert!(summary.get("payload").is_none());
        assert!(!serialized.contains("PRIVATE_RAW_TRACE_BODY"));
        assert!(!serialized.contains("PRIVATE_OUTPUT"));
        assert!(!serialized.contains("private_token"));
    }

    #[test]
    fn skill_runtime_audit_results_are_metadata_only() {
        let list = session_log_result_for_tool(
            "skill_list",
            &json!({
                "project": "agent:test:demo",
                "catalog_revision": "wc_skillcat_deadbeef",
                "total_count": 1,
                "returned_count": 1,
                "truncated": false,
                "invalid_count": 0,
                "discovery_truncated": false,
                "skills": [{"name": "PRIVATE DESCRIPTION", "description": "PRIVATE CATALOG BODY"}]
            }),
        );
        let list_serialized = serde_json::to_string(&list).unwrap();
        assert!(!list_serialized.contains("PRIVATE DESCRIPTION"));
        assert!(!list_serialized.contains("PRIVATE CATALOG BODY"));
        assert!(list.get("skills").is_none());

        let read = session_log_result_for_tool(
            "skill_read_file",
            &json!({
                "project": "agent:test:demo",
                "skill_id": "wc_skill_0123456789abcdef0123456789abcdef",
                "definition_revision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "path": "SKILL.md",
                "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "text": "PRIVATE_SKILL_BODY",
                "start_line": 1,
                "end_line": 2,
                "returned_lines": 2,
                "has_more": false,
                "next_start_line": null
            }),
        );
        let read_serialized = serde_json::to_string(&read).unwrap();
        assert!(!read_serialized.contains("PRIVATE_SKILL_BODY"));
        assert!(read.get("text").is_none());
        assert_eq!(read["path"], "SKILL.md");
        assert_eq!(read["returned_lines"], 2);
    }

    #[test]
    fn skill_management_audit_omits_paths_keys_and_package_bodies() {
        let args = session_log_arguments_for_tool_request(
            "skill_install",
            &json!({
                "project": "agent:test:demo",
                "skill_key": "demo",
                "artifact_path": "artifacts/PRIVATE_PACKAGE_NAME.zip",
                "expected_artifact_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "idempotency_key": "PRIVATE_IDEMPOTENCY_KEY",
                "activate": true,
                "expected_state_revision": "wc_skillstate_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            }),
        );
        let args_serialized = serde_json::to_string(&args).unwrap();
        assert!(!args_serialized.contains("PRIVATE_PACKAGE_NAME"));
        assert!(!args_serialized.contains("PRIVATE_IDEMPOTENCY_KEY"));
        assert_eq!(args["artifact_path_present"], true);
        assert_eq!(args["idempotency_key_present"], true);

        let typed_args = ToolCall::SkillInstall {
            project: "agent:test:demo".to_string(),
            skill_key: "demo".to_string(),
            artifact_path: "artifacts/PRIVATE_PACKAGE_NAME.zip".to_string(),
            expected_artifact_sha256:
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            idempotency_key: "PRIVATE_IDEMPOTENCY_KEY".to_string(),
            activate: Some(true),
            expected_state_revision: Some(
                "wc_skillstate_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .to_string(),
            ),
            session_id: None,
        }
        .session_log_arguments();
        let typed_serialized = serde_json::to_string(&typed_args).unwrap();
        assert!(!typed_serialized.contains("PRIVATE_PACKAGE_NAME"));
        assert!(!typed_serialized.contains("PRIVATE_IDEMPOTENCY_KEY"));
        assert_eq!(typed_args["skill_key"], "demo");
        assert_eq!(typed_args["artifact_path_present"], true);
        assert_eq!(typed_args["idempotency_key_present"], true);

        let versions = session_log_result_for_tool(
            "skill_versions",
            &json!({
                "project": "agent:test:demo",
                "skill_id": "wc_skill_0123456789abcdef0123456789abcdef",
                "skill_key": "demo",
                "state_revision": "wc_skillstate_cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                "active_package_revision": "wc_skillpkg_dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                "total_count": 1,
                "offset": 0,
                "next_offset": null,
                "versions": [{
                    "description": "PRIVATE_REVISION_DESCRIPTION",
                    "native_store_path": "/PRIVATE/NATIVE/STORE/PATH"
                }]
            }),
        );
        let versions_serialized = serde_json::to_string(&versions).unwrap();
        assert!(!versions_serialized.contains("PRIVATE_REVISION_DESCRIPTION"));
        assert!(!versions_serialized.contains("PRIVATE/NATIVE/STORE"));
        assert!(versions.get("versions").is_none());

        let install = session_log_result_for_tool(
            "skill_install",
            &json!({
                "project": "agent:test:demo",
                "skill_id": "wc_skill_0123456789abcdef0123456789abcdef",
                "skill_key": "demo",
                "package_revision": "wc_skillpkg_dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                "definition_revision": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                "artifact_sha256": "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                "file_count": 2,
                "total_bytes": 123,
                "installed": true,
                "activated": false,
                "replayed": false,
                "state_revision": "wc_skillstate_cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                "active_package_revision": null,
                "raw_skill_body": "PRIVATE_SKILL_BODY",
                "archive_bytes": "PRIVATE_ZIP_BYTES",
                "native_store_path": "/PRIVATE/NATIVE/STORE/PATH",
                "staging_path": "/PRIVATE/STAGING/PATH"
            }),
        );
        let install_serialized = serde_json::to_string(&install).unwrap();
        for private in [
            "PRIVATE_SKILL_BODY",
            "PRIVATE_ZIP_BYTES",
            "PRIVATE/NATIVE/STORE",
            "PRIVATE/STAGING/PATH",
        ] {
            assert!(!install_serialized.contains(private), "leaked {private}");
        }
    }

    #[test]
    fn communication_audit_omits_profile_and_message_bodies() {
        const PRIVATE_HANDLE: &str = "PRIVATE_HANDLE";
        const PRIVATE_DISPLAY: &str = "PRIVATE DISPLAY";
        const PRIVATE_DESCRIPTION: &str = "PRIVATE AGENT DESCRIPTION";
        const PRIVATE_LABEL: &str = "PRIVATE_LABEL";
        const PRIVATE_BODY: &str = "PRIVATE CONVERSATION BODY";
        const PRIVATE_KEY: &str = "PRIVATE_IDEMPOTENCY_KEY";

        let create = ToolCall::CreateAgentIdentity {
            handle: PRIVATE_HANDLE.to_string(),
            display_name: PRIVATE_DISPLAY.to_string(),
            description: Some(PRIVATE_DESCRIPTION.to_string()),
            specialty_labels: vec![PRIVATE_LABEL.to_string()],
            idempotency_key: PRIVATE_KEY.to_string(),
        }
        .session_log_arguments();
        let create_text = create.to_string();
        for private in [
            PRIVATE_HANDLE,
            PRIVATE_DISPLAY,
            PRIVATE_DESCRIPTION,
            PRIVATE_LABEL,
            PRIVATE_KEY,
        ] {
            assert!(
                !create_text.contains(private),
                "create audit leaked {private}"
            );
        }
        assert_eq!(create["description_bytes"], PRIVATE_DESCRIPTION.len());
        assert_eq!(create["specialty_label_count"], 1);
        assert_eq!(create["idempotency_key_present"], true);

        let post = ToolCall::PostConversationMessage {
            conversation_id: "wc_conv_0123456789abcdef0123456789abcdef".to_string(),
            body: PRIVATE_BODY.to_string(),
            author_agent_id: None,
            endpoint_id: None,
            expected_controller_generation: None,
            recipient_agent_ids: Some(vec![
                "wc_dagent_0123456789abcdef0123456789abcdef".to_string()
            ]),
            reply_to: None,
            idempotency_key: Some(PRIVATE_KEY.to_string()),
            wake_reply_id: None,
            reply_operation_index: None,
        }
        .session_log_arguments();
        let post_text = post.to_string();
        assert!(!post_text.contains(PRIVATE_BODY));
        assert!(!post_text.contains(PRIVATE_KEY));
        assert_eq!(post["body_bytes"], PRIVATE_BODY.len());
        assert_eq!(post["recipient_count"], 1);

        let result = session_log_result_for_tool(
            "post_conversation_message",
            &json!({
                "message": {
                    "message_id": "wc_cmsg_0123456789abcdef0123456789abcdef",
                    "conversation_id": "wc_conv_0123456789abcdef0123456789abcdef",
                    "seq": 4,
                    "body": PRIVATE_BODY,
                    "deliveries": [{"delivery_id": "wc_delivery_0123456789abcdef0123456789abcdef"}]
                },
                "replayed": false,
                "state_changed": true
            }),
        );
        assert!(!result.to_string().contains(PRIVATE_BODY));
        assert_eq!(result["seq"], 4);
        assert_eq!(result["delivery_count"], 1);

        let bootstrap = session_log_result_for_tool(
            "bootstrap_agent_conversation",
            &json!({
                "acting_agent": {
                    "agent_id": "wc_dagent_0123456789abcdef0123456789abcdef",
                    "description": PRIVATE_DESCRIPTION,
                    "specialty_labels": [PRIVATE_LABEL]
                },
                "endpoint": {
                    "endpoint_id": "wc_endpoint_0123456789abcdef0123456789abcdef",
                    "controller_generation": 4,
                    "client_attachment_id": "PRIVATE_HOST_ATTACHMENT"
                },
                "selected_conversation": {
                    "conversation_id": "wc_conv_0123456789abcdef0123456789abcdef"
                },
                "inbox": {"queued_delivery_count": 2},
                "wake": {
                    "wake_id": "wc_wake_0123456789abcdef0123456789abcdef",
                    "state": "pending",
                    "consume_token": "PRIVATE_CONSUME_TOKEN",
                    "message_body": PRIVATE_BODY
                },
                "host_binding": {
                    "adapter_kind": "host_adapter",
                    "runtime_wake_capable": true,
                    "production_auto_resume_available": false,
                    "callback_secret": "PRIVATE_CALLBACK_SECRET"
                },
                "wake_activation": {
                    "wake_id": "wc_wake_0123456789abcdef0123456789abcdef",
                    "attempt_id": "wc_wake_attempt_0123456789abcdef0123456789abcdef",
                    "consume_token": "PRIVATE_ACTIVATION_CONSUME_TOKEN",
                    "adapter_kind": "explicit_activation"
                }
            }),
        );
        let bootstrap_text = bootstrap.to_string();
        for private in [
            PRIVATE_DESCRIPTION,
            PRIVATE_LABEL,
            PRIVATE_BODY,
            "PRIVATE_HOST_ATTACHMENT",
            "PRIVATE_CONSUME_TOKEN",
            "PRIVATE_CALLBACK_SECRET",
            "PRIVATE_ACTIVATION_CONSUME_TOKEN",
        ] {
            assert!(
                !bootstrap_text.contains(private),
                "bootstrap audit leaked {private}"
            );
        }
        assert_eq!(bootstrap["controller_generation"], 4);
        assert_eq!(bootstrap["queued_delivery_count"], 2);

        let activation_request = ToolCall::BootstrapAgentConversation {
            agent_id: "wc_dagent_0123456789abcdef0123456789abcdef".to_string(),
            endpoint_id: "wc_endpoint_0123456789abcdef0123456789abcdef".to_string(),
            expected_controller_generation: 4,
            conversation_id: None,
            wake_id: Some("wc_wake_0123456789abcdef0123456789abcdef".to_string()),
            activation_idempotency_key: Some(PRIVATE_KEY.to_string()),
        }
        .session_log_arguments();
        assert!(!activation_request.to_string().contains(PRIVATE_KEY));
    }

    #[test]
    fn agent_wake_consume_audit_omits_raw_consume_token_and_payload_fields() {
        const PRIVATE_TOKEN: &str = "wc_wake_consume_PRIVATE_TOKEN_MUST_NOT_PERSIST";
        const PRIVATE_BODY: &str = "PRIVATE_WAKE_PAYLOAD_BODY";
        const PRIVATE_DESCRIPTION: &str = "PRIVATE_AGENT_DESCRIPTION";
        const PRIVATE_DIGEST: &str = "PRIVATE_PRINCIPAL_DIGEST";
        const PRIVATE_KEY: &str = "PRIVATE_IDEMPOTENCY_KEY";

        let request = session_log_arguments_for_tool_request(
            "consume_agent_wake",
            &json!({
                "agent_id": "wc_dagent_0123456789abcdef0123456789abcdef",
                "endpoint_id": "wc_endpoint_0123456789abcdef0123456789abcdef",
                "expected_controller_generation": 7,
                "wake_id": "wc_wake_0123456789abcdef0123456789abcdef",
                "consume_token": PRIVATE_TOKEN,
                "body": PRIVATE_BODY,
                "description": PRIVATE_DESCRIPTION,
                "principal_digest": PRIVATE_DIGEST,
                "idempotency_key": PRIVATE_KEY
            }),
        );
        assert_eq!(request["consume_token_present"], true);
        assert_eq!(request["expected_controller_generation"], 7);
        let typed_request = ToolCall::ConsumeAgentWake {
            agent_id: "wc_dagent_0123456789abcdef0123456789abcdef".to_string(),
            endpoint_id: "wc_endpoint_0123456789abcdef0123456789abcdef".to_string(),
            expected_controller_generation: 7,
            wake_id: "wc_wake_0123456789abcdef0123456789abcdef".to_string(),
            consume_token: PRIVATE_TOKEN.to_string(),
        }
        .session_log_arguments();
        assert_eq!(typed_request["consume_token_present"], true);
        assert!(!typed_request.to_string().contains(PRIVATE_TOKEN));
        let request_text = request.to_string();
        for private in [
            PRIVATE_TOKEN,
            PRIVATE_BODY,
            PRIVATE_DESCRIPTION,
            PRIVATE_DIGEST,
            PRIVATE_KEY,
        ] {
            assert!(
                !request_text.contains(private),
                "wake consume audit leaked {private}"
            );
        }

        let result = session_log_result_for_tool(
            "consume_agent_wake",
            &json!({
                "wake_id": "wc_wake_0123456789abcdef0123456789abcdef",
                "target_agent_id": "wc_dagent_0123456789abcdef0123456789abcdef",
                "state": "consumed",
                "already_consumed": false,
                "consumed_at_unix_ms": 123,
                "state_changed": true,
                "consume_token": PRIVATE_TOKEN,
                "body": PRIVATE_BODY,
                "description": PRIVATE_DESCRIPTION,
                "principal_digest": PRIVATE_DIGEST,
                "idempotency_key": PRIVATE_KEY
            }),
        );
        let result_text = result.to_string();
        for private in [
            PRIVATE_TOKEN,
            PRIVATE_BODY,
            PRIVATE_DESCRIPTION,
            PRIVATE_DIGEST,
            PRIVATE_KEY,
        ] {
            assert!(
                !result_text.contains(private),
                "wake consume result audit leaked {private}"
            );
        }
        assert_eq!(result["state"], "consumed");
        assert_eq!(result["state_changed"], true);
    }

    #[test]
    fn memory_audit_is_metadata_only_for_search_read_set_and_delete() {
        let private_query = "PRIVATE_MEMORY_QUERY";
        let private_summary = "PRIVATE_MEMORY_SUMMARY";
        let private_body = "PRIVATE_MEMORY_BODY";
        let private_tag = "PRIVATE_MEMORY_TAG";
        let revision = format!("wc_memrev_{}", "a".repeat(64));
        let memory_id = "wc_mem_0123456789abcdef0123456789abcdef";

        let search_args = session_log_arguments_for_tool_request(
            "memory_search",
            &json!({
                "project": "agent:test:demo",
                "query": private_query,
                "tags": [private_tag],
                "limit": 10
            }),
        );
        let search_args_serialized = search_args.to_string();
        assert!(!search_args_serialized.contains(private_query));
        assert!(!search_args_serialized.contains(private_tag));
        assert_eq!(search_args["query_present"], true);
        assert_eq!(search_args["tag_count"], 1);

        let set_args = session_log_arguments_for_tool_request(
            "memory_set",
            &json!({
                "project": "agent:test:demo",
                "memory_key": "policy",
                "summary": private_summary,
                "body": private_body,
                "priority": "high",
                "bootstrap": true,
                "tags": [private_tag]
            }),
        );
        let set_args_serialized = set_args.to_string();
        for private in [private_summary, private_body, private_tag] {
            assert!(!set_args_serialized.contains(private));
        }
        assert_eq!(set_args["summary_present"], true);
        assert_eq!(set_args["body_present"], true);
        assert_eq!(set_args["tag_count"], 1);

        let typed_set = ToolCall::MemorySet {
            project: "agent:test:demo".to_string(),
            memory_key: "policy".to_string(),
            summary: private_summary.to_string(),
            body: Some(private_body.to_string()),
            priority: Some("high".to_string()),
            bootstrap: Some(true),
            tags: Some(vec![private_tag.to_string()]),
            expected_revision: None,
            session_id: None,
        }
        .session_log_arguments();
        let typed_set_serialized = typed_set.to_string();
        for private in [private_summary, private_body, private_tag] {
            assert!(!typed_set_serialized.contains(private));
        }

        let search_result = session_log_result_for_tool(
            "memory_search",
            &json!({
                "project": "agent:test:demo",
                "catalog_revision": format!("wc_memcat_{}", "b".repeat(64)),
                "total_count": 1,
                "returned_count": 1,
                "truncated": false,
                "memories": [{
                    "memory_id": memory_id,
                    "memory_key": "policy",
                    "summary": private_summary,
                    "tags": [private_tag],
                    "revision": revision
                }]
            }),
        );
        let search_result_serialized = search_result.to_string();
        assert!(!search_result_serialized.contains(private_summary));
        assert!(!search_result_serialized.contains(private_tag));
        assert!(search_result.get("memories").is_none());

        let read_result = session_log_result_for_tool(
            "memory_read",
            &json!({
                "project": "agent:test:demo",
                "memory_id": memory_id,
                "memory_key": "policy",
                "summary": private_summary,
                "body": private_body,
                "priority": "high",
                "bootstrap": true,
                "tags": [private_tag],
                "revision": revision
            }),
        );
        let read_result_serialized = read_result.to_string();
        for private in [private_summary, private_body, private_tag] {
            assert!(!read_result_serialized.contains(private));
        }
        assert_eq!(read_result["returned_body_bytes"], private_body.len());

        let set_result = session_log_result_for_tool(
            "memory_set",
            &json!({
                "project": "agent:test:demo",
                "memory_id": memory_id,
                "memory_key": "policy",
                "revision": revision,
                "created": true,
                "state_changed": true,
                "summary": private_summary,
                "body": private_body,
                "tags": [private_tag]
            }),
        );
        let set_result_serialized = set_result.to_string();
        for private in [private_summary, private_body, private_tag] {
            assert!(!set_result_serialized.contains(private));
        }

        let delete_result = session_log_result_for_tool(
            "memory_delete",
            &json!({
                "project": "agent:test:demo",
                "memory_id": memory_id,
                "memory_key": "policy",
                "revision": revision,
                "deleted": true,
                "state_changed": true,
                "body": private_body
            }),
        );
        let private_principal_digest = format!("wc_memprincipal_{}", "d".repeat(64));
        let private_native_root = "/PRIVATE/NATIVE/MEMORY/ROOT";
        let scope_id = format!("wc_memscope_{}", "c".repeat(64));
        let catalog_revision = format!("wc_memcat_{}", "e".repeat(64));
        let purge_args = session_log_arguments_for_tool_request(
            "memory_scope_purge",
            &json!({
                "memory_scope_id": scope_id,
                "expected_catalog_revision": catalog_revision,
                "confirm": true,
                "body": private_body,
            }),
        );
        assert_eq!(purge_args["memory_scope_id"], scope_id);
        assert_eq!(purge_args["expected_catalog_revision"], catalog_revision);
        assert!(purge_args.get("confirm").is_none());
        assert!(!purge_args.to_string().contains(private_body));
        let typed_purge = ToolCall::MemoryScopePurge {
            memory_scope_id: scope_id.clone(),
            expected_catalog_revision: catalog_revision.clone(),
            confirm: true,
        }
        .session_log_arguments();
        assert!(typed_purge.get("confirm").is_none());

        let scope_list = session_log_result_for_tool(
            "memory_scope_list",
            &json!({
                "total_count": 1,
                "returned_count": 1,
                "truncated": false,
                "scopes": [{
                    "memory_scope_id": scope_id,
                    "identity_state": "attributed",
                    "current_status": "not_current",
                    "catalog_revision": catalog_revision,
                    "memory_count": 1,
                    "summary": private_summary,
                    "body": private_body,
                    "tags": [private_tag],
                    "native_root": private_native_root,
                    "principal_digest": private_principal_digest
                }]
            }),
        );
        let scope_list_text = scope_list.to_string();
        assert_eq!(scope_list["total_count"], 1);
        assert_eq!(scope_list["returned_count"], 1);
        assert!(scope_list.get("scopes").is_none());
        for private in [
            private_summary,
            private_body,
            private_tag,
            private_native_root,
            private_principal_digest.as_str(),
        ] {
            assert!(
                !scope_list_text.contains(private),
                "scope-list audit leaked {private}"
            );
        }

        let purge = session_log_result_for_tool(
            "memory_scope_purge",
            &json!({
                "memory_scope_id": scope_id,
                "catalog_revision": catalog_revision,
                "purged_count": 1,
                "purged": true,
                "state_changed": true,
                "summary": private_summary,
                "body": private_body,
                "tags": [private_tag],
                "native_root": private_native_root,
                "principal_digest": private_principal_digest
            }),
        );
        let purge_text = purge.to_string();
        assert_eq!(purge["memory_scope_id"], scope_id);
        assert_eq!(purge["catalog_revision"], catalog_revision);
        assert_eq!(purge["purged_count"], 1);
        assert!(purge.get("purged").is_none());
        assert_eq!(purge["state_changed"], true);
        for private in [
            private_summary,
            private_body,
            private_tag,
            private_native_root,
            private_principal_digest.as_str(),
        ] {
            assert!(
                !purge_text.contains(private),
                "purge audit leaked {private}"
            );
        }
        assert!(!delete_result.to_string().contains(private_body));
    }

    #[test]
    fn computer_display_list_ledger_omits_ids_and_native_topology() {
        let output = json!({
            "displays": [{
                "display_id": "display_0123456789abcdef0123456789abcdef",
                "width": 1920,
                "height": 1080,
                "primary": true,
                "native_identity": "PRIVATE_NATIVE_ID",
                "device_path": "PRIVATE_DEVICE_PATH",
                "global_x": -1920
            }],
            "count": 1,
            "truncated": false
        });
        let summary = session_log_result_for_tool("computer_list_displays", &output);
        let serialized = serde_json::to_string(&summary).unwrap();
        assert_eq!(summary, json!({"count": 1, "truncated": false}));
        assert!(!serialized.contains("display_"));
        assert!(!serialized.contains("PRIVATE_NATIVE_ID"));
        assert!(!serialized.contains("PRIVATE_DEVICE_PATH"));
        assert!(!serialized.contains("global_x"));
    }

    #[test]
    fn computer_display_snapshot_ledger_omits_image_and_native_topology() {
        let display_id = "display_0123456789abcdef0123456789abcdef";
        let request = json!({
            "client_id": "msi",
            "display_id": display_id,
            "max_width": 1024,
            "max_height": 768,
            "global_x": -1920
        });
        let request_summary =
            session_log_arguments_for_tool_request("computer_snapshot_display", &request);
        assert_eq!(request_summary["display_id"], display_id);
        assert!(request_summary.get("global_x").is_none());

        let output = json!({
            "display_id": display_id,
            "snapshot_generation": 9,
            "source_width": 1920,
            "source_height": 1080,
            "width": 1024,
            "height": 576,
            "mime_type": "image/jpeg",
            "file_bytes": 1234,
            "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "captured_at_unix_ms": 1_700_000_000_000u64,
            "content_base64": "PRIVATE_IMAGE_BODY",
            "native_identity": "PRIVATE_NATIVE_ID",
            "device_path": "PRIVATE_DEVICE_PATH",
            "global_x": -1920,
            "scale_factor": 1.25
        });
        let summary = session_log_result_for_tool("computer_snapshot_display", &output);
        let serialized = serde_json::to_string(&summary).unwrap();
        assert_eq!(summary["display_id"], display_id);
        assert_eq!(summary["snapshot_generation"], 9);
        assert_eq!(summary["sha256"], output["sha256"]);
        assert!(!serialized.contains("PRIVATE_IMAGE_BODY"));
        assert!(!serialized.contains("PRIVATE_NATIVE_ID"));
        assert!(!serialized.contains("PRIVATE_DEVICE_PATH"));
        assert!(!serialized.contains("global_x"));
        assert!(!serialized.contains("scale_factor"));
    }

    #[test]
    fn computer_clipboard_ledger_omits_body_hashes_and_native_state() {
        const PRIVATE_TEXT: &str = "PRIVATE_CLIPBOARD_TEXT";
        let read_request = json!({
            "client_id": "msi",
            "text": PRIVATE_TEXT,
            "hwnd": "PRIVATE_HWND",
        });
        let read_request_summary =
            session_log_arguments_for_tool_request("computer_read_clipboard", &read_request);
        assert_eq!(read_request_summary, json!({"client_id":"msi"}));

        let write_request = json!({
            "client_id": "msi",
            "text": PRIVATE_TEXT,
            "sha256": "PRIVATE_CLIPBOARD_HASH",
            "native_handle": "PRIVATE_HGLOBAL",
        });
        let write_request_summary =
            session_log_arguments_for_tool_request("computer_write_clipboard", &write_request);
        assert_eq!(write_request_summary["client_id"], "msi");
        assert_eq!(write_request_summary["text_bytes"], PRIVATE_TEXT.len());
        let request_serialized = serde_json::to_string(&write_request_summary).unwrap();
        for secret in [PRIVATE_TEXT, "PRIVATE_CLIPBOARD_HASH", "PRIVATE_HGLOBAL"] {
            assert!(!request_serialized.contains(secret));
        }

        let read_output = json!({
            "available": true,
            "text": PRIVATE_TEXT,
            "text_bytes": PRIVATE_TEXT.len(),
            "success": true,
            "error_kind": null,
            "execution_state": null,
            "sha256": "PRIVATE_CLIPBOARD_HASH",
            "hwnd": "PRIVATE_HWND",
            "native_owner": "PRIVATE_OWNER",
        });
        let read_summary = session_log_result_for_tool("computer_read_clipboard", &read_output);
        assert_eq!(read_summary["available"], true);
        assert_eq!(read_summary["text_bytes"], PRIVATE_TEXT.len());
        let read_serialized = serde_json::to_string(&read_summary).unwrap();
        for secret in [
            PRIVATE_TEXT,
            "PRIVATE_CLIPBOARD_HASH",
            "PRIVATE_HWND",
            "PRIVATE_OWNER",
        ] {
            assert!(!read_serialized.contains(secret));
        }

        let write_output = json!({
            "text_bytes": PRIVATE_TEXT.len(),
            "success": true,
            "error_kind": null,
            "execution_state": "completed",
            "state_changed": true,
            "text": PRIVATE_TEXT,
            "sha256": "PRIVATE_CLIPBOARD_HASH",
            "hglobal": "PRIVATE_HGLOBAL",
            "clipboard_owner": "PRIVATE_OWNER",
        });
        let write_summary = session_log_result_for_tool("computer_write_clipboard", &write_output);
        assert_eq!(write_summary["text_bytes"], PRIVATE_TEXT.len());
        assert_eq!(write_summary["success"], true);
        let write_serialized = serde_json::to_string(&write_summary).unwrap();
        for secret in [
            PRIVATE_TEXT,
            "PRIVATE_CLIPBOARD_HASH",
            "PRIVATE_HGLOBAL",
            "PRIVATE_OWNER",
        ] {
            assert!(!write_serialized.contains(secret));
        }
    }

    #[test]
    fn computer_pointer_ledger_keeps_only_source_space_and_opaque_lifecycle_metadata() {
        let display_id = "display_0123456789abcdef0123456789abcdef";
        let request = json!({
            "client_id": "msi",
            "display_id": display_id,
            "snapshot_generation": 11,
            "x": 321,
            "y": 654,
            "global_x": -1599,
            "native_identity": "PRIVATE_NATIVE_ID"
        });
        let request_summary =
            session_log_arguments_for_tool_request("computer_pointer_click", &request);
        let request_serialized = serde_json::to_string(&request_summary).unwrap();
        assert_eq!(request_summary["display_id"], display_id);
        assert_eq!(request_summary["snapshot_generation"], 11);
        assert_eq!(request_summary["x"], 321);
        assert_eq!(request_summary["y"], 654);
        assert!(!request_serialized.contains("global_x"));
        assert!(!request_serialized.contains("PRIVATE_NATIVE_ID"));

        let output = json!({
            "display_id": display_id,
            "snapshot_generation": 11,
            "x": 321,
            "y": 654,
            "success": true,
            "error_kind": null,
            "execution_state": "completed",
            "state_changed": true,
            "content_base64": "PRIVATE_IMAGE_BODY",
            "native_identity": "PRIVATE_NATIVE_ID",
            "device_path": "PRIVATE_DEVICE_PATH",
            "global_x": -1599,
            "virtual_left": -1920,
            "dpi_scale": 1.25,
            "bounds": [0.0, 0.0, 1920.0, 1080.0],
            "rotation": 0.0,
            "event_source": "CombinedSessionState",
            "cursor_native_x": 160.5,
            "held_buttons": 0
        });
        let summary = session_log_result_for_tool("computer_pointer_click", &output);
        let serialized = serde_json::to_string(&summary).unwrap();
        assert_eq!(summary["display_id"], display_id);
        assert_eq!(summary["snapshot_generation"], 11);
        assert_eq!(summary["x"], 321);
        assert_eq!(summary["y"], 654);
        for secret in [
            "PRIVATE_IMAGE_BODY",
            "PRIVATE_NATIVE_ID",
            "PRIVATE_DEVICE_PATH",
            "global_x",
            "virtual_left",
            "dpi_scale",
            "bounds",
            "rotation",
            "event_source",
            "cursor_native_x",
            "held_buttons",
        ] {
            assert!(!serialized.contains(secret), "{secret}");
        }
    }

    #[test]
    fn computer_application_launch_ledger_keeps_only_opaque_lifecycle_metadata() {
        let application_id = "application_0123456789abcdef0123456789abcdef";
        let output = json!({
            "application_id": application_id,
            "success": true,
            "error_kind": null,
            "execution_state": null,
            "state_changed": null,
            "native_identity": "PRIVATE_NATIVE_ID",
            "path": "C:\\Private\\app.exe",
            "display_name": "Private App"
        });
        let summary = session_log_result_for_tool("computer_launch_application", &output);
        assert_eq!(
            summary,
            json!({
                "application_id": application_id,
                "success": true,
                "error_kind": null,
                "execution_state": null,
                "state_changed": null
            })
        );
        let serialized = serde_json::to_string(&summary).unwrap();
        assert!(!serialized.contains("PRIVATE_NATIVE_ID"));
        assert!(!serialized.contains("Private App"));
        assert!(!serialized.contains("app.exe"));
    }

    #[test]
    fn computer_list_ledger_result_omits_window_content() {
        let output = json!({
            "windows": [{
                "surface_id": "surface_secret",
                "application": "Private App",
                "title": "Confidential Window Title",
                "width": 1200,
                "height": 800,
                "focused": true,
                "active": true
            }],
            "count": 1,
            "truncated": false
        });
        let summary = session_log_result_for_tool("computer_list_windows", &output);
        let serialized = serde_json::to_string(&summary).unwrap();
        assert_eq!(summary, json!({"count": 1, "truncated": false}));
        assert!(!serialized.contains("Confidential"));
        assert!(!serialized.contains("Private App"));
        assert!(!serialized.contains("surface_secret"));
    }

    #[test]
    fn computer_accessibility_tree_ledger_result_omits_semantic_content() {
        let output = json!({
            "platform": "macos",
            "surface_id": "surface_safe",
            "nodes": [{
                "element_id": "element_secret",
                "parent_element_id": null,
                "depth": 0,
                "role": "AXWindow",
                "subrole": null,
                "title": "Private Chat",
                "description": "Confidential",
                "value": "SUPER_SECRET_MESSAGE",
                "placeholder": null,
                "enabled": true,
                "focused": false,
                "child_count": 2
            }],
            "node_count": 1,
            "truncated": true,
            "max_depth": 6,
            "max_nodes": 128
        });
        let summary = session_log_result_for_tool("computer_accessibility_tree", &output);
        let serialized = serde_json::to_string(&summary).unwrap();
        assert_eq!(summary["surface_id"], "surface_safe");
        assert_eq!(summary["node_count"], 1);
        assert!(!serialized.contains("SUPER_SECRET"));
        assert!(!serialized.contains("Private Chat"));
        assert!(!serialized.contains("element_secret"));
    }

    #[test]
    fn computer_find_elements_audit_omits_label_and_semantic_result_content() {
        let secret = "PRIVATE SEARCH TERM";
        let private_role = "PRIVATE ROLE FILTER";
        let private_subrole = "PRIVATE SUBROLE FILTER";
        let request = json!({
            "client_id": "mini",
            "surface_id": "surface_safe",
            "role": private_role,
            "subrole": private_subrole,
            "label": secret,
            "focused": false,
            "limit": 4,
        });
        let request_summary =
            session_log_arguments_for_tool_request("computer_find_elements", &request);
        let request_serialized = serde_json::to_string(&request_summary).unwrap();
        assert_eq!(request_summary["client_id"], "mini");
        assert_eq!(request_summary["surface_id"], "surface_safe");
        assert_eq!(request_summary["role_present"], true);
        assert_eq!(request_summary["subrole_present"], true);
        assert_eq!(request_summary["label_present"], true);
        assert!(!request_serialized.contains(secret));
        assert!(!request_serialized.contains(private_role));
        assert!(!request_serialized.contains(private_subrole));

        let parsed_summary = ToolCall::ComputerFindElements {
            client_id: "mini".to_string(),
            surface_id: "surface_safe".to_string(),
            role: Some(private_role.to_string()),
            subrole: Some(private_subrole.to_string()),
            label: Some(secret.to_string()),
            focused: Some(false),
            enabled: None,
            limit: Some(4),
        }
        .session_log_arguments();
        let parsed_serialized = serde_json::to_string(&parsed_summary).unwrap();
        assert_eq!(parsed_summary["role_present"], true);
        assert_eq!(parsed_summary["subrole_present"], true);
        assert_eq!(parsed_summary["label_present"], true);
        assert!(!parsed_serialized.contains(secret));
        assert!(!parsed_serialized.contains(private_role));
        assert!(!parsed_serialized.contains(private_subrole));

        let output = json!({
            "platform": "macos",
            "surface_id": "surface_safe",
            "elements": [{
                "element_id": "element_secret",
                "role": "AXTextField",
                "subrole": "AXSearchField",
                "title": "Private Search",
                "description": "Confidential",
                "placeholder": secret,
                "enabled": true,
                "focused": false
            }],
            "count": 1,
            "scanned_nodes": 18,
            "truncated": false
        });
        let result_summary = session_log_result_for_tool("computer_find_elements", &output);
        let result_serialized = serde_json::to_string(&result_summary).unwrap();
        assert_eq!(result_summary["surface_id"], "surface_safe");
        assert_eq!(result_summary["count"], 1);
        assert_eq!(result_summary["scanned_nodes"], 18);
        assert!(!result_serialized.contains(secret));
        assert!(!result_serialized.contains("element_secret"));
        assert!(!result_serialized.contains("Private Search"));
    }

    #[test]
    fn computer_element_state_ledger_omits_content_derived_state() {
        let request = json!({
            "client_id": "mini",
            "surface_id": "surface_safe",
            "element_id": "element_safe",
        });
        let request_summary =
            session_log_arguments_for_tool_request("computer_element_state", &request);
        assert_eq!(request_summary, request);

        let output = json!({
            "platform": "macos",
            "surface_id": "surface_safe",
            "element_id": "element_safe",
            "observation_generation": 9,
            "enabled": true,
            "focused": true,
            "protected": false,
            "value_empty": false,
            "can_press": true,
            "can_focus": true,
            "can_input_text": false
        });
        let summary = session_log_result_for_tool("computer_element_state", &output);
        assert_eq!(summary["surface_id"], "surface_safe");
        assert_eq!(summary["element_id"], "element_safe");
        assert_eq!(summary["observation_generation"], 9);
        for field in [
            "enabled",
            "focused",
            "protected",
            "value_empty",
            "can_press",
            "can_focus",
            "can_input_text",
        ] {
            assert!(summary.get(field).is_none(), "audit leaked {field}");
        }
    }

    #[test]
    fn computer_activate_window_ledger_is_exact_metadata_only() {
        let request = json!({
            "client_id": "mini",
            "surface_id": "surface_safe",
        });
        let request_summary =
            session_log_arguments_for_tool_request("computer_activate_window", &request);
        assert_eq!(request_summary, request);

        let output = json!({
            "platform": "macos",
            "surface_id": "surface_safe",
            "success": true,
            "application": "PRIVATE APP",
            "title": "PRIVATE WINDOW"
        });
        let summary = session_log_result_for_tool("computer_activate_window", &output);
        let serialized = serde_json::to_string(&summary).unwrap();
        assert_eq!(summary["surface_id"], "surface_safe");
        assert_eq!(summary["success"], true);
        assert!(!serialized.contains("PRIVATE APP"));
        assert!(!serialized.contains("PRIVATE WINDOW"));
    }

    #[test]
    fn computer_control_ledger_result_is_metadata_only() {
        // Control remains metadata-only independently of CU-AX3.
        let output = json!({
            "platform": "macos",
            "surface_id": "surface_safe",
            "element_id": "element_safe",
            "action": "press",
            "success": true,
            "title": "PRIVATE CONTROL TARGET",
            "value": "SUPER_SECRET_VALUE"
        });
        let summary = session_log_result_for_tool("computer_control", &output);
        let serialized = serde_json::to_string(&summary).unwrap();
        assert_eq!(summary["surface_id"], "surface_safe");
        assert_eq!(summary["element_id"], "element_safe");
        assert_eq!(summary["action"], "press");
        assert_eq!(summary["success"], true);
        assert!(!serialized.contains("PRIVATE CONTROL TARGET"));
        assert!(!serialized.contains("SUPER_SECRET_VALUE"));
    }

    #[test]
    fn computer_scroll_to_element_ledger_is_metadata_only() {
        let request = json!({
            "client_id": "mini",
            "surface_id": "surface_safe",
            "element_id": "element_safe",
        });
        let request_summary =
            session_log_arguments_for_tool_request("computer_scroll_to_element", &request);
        assert_eq!(request_summary, request);

        let output = json!({
            "platform": "macos",
            "surface_id": "surface_safe",
            "element_id": "element_safe",
            "success": true,
            "title": "PRIVATE SCROLLED TARGET",
            "value": "SUPER_SECRET_VALUE"
        });
        let summary = session_log_result_for_tool("computer_scroll_to_element", &output);
        let serialized = serde_json::to_string(&summary).unwrap();
        assert_eq!(summary["surface_id"], "surface_safe");
        assert_eq!(summary["element_id"], "element_safe");
        assert_eq!(summary["success"], true);
        assert!(!serialized.contains("PRIVATE SCROLLED TARGET"));
        assert!(!serialized.contains("SUPER_SECRET_VALUE"));
    }

    #[test]
    fn computer_key_input_ledger_is_closed_metadata_only() {
        let request = json!({
            "client_id": "mini",
            "surface_id": "surface_safe",
            "key": "tab",
            "modifiers": ["shift"],
            "text": "MUST_NOT_PERSIST",
            "keycode": 123
        });
        let request_summary =
            session_log_arguments_for_tool_request("computer_key_input", &request);
        let request_serialized = serde_json::to_string(&request_summary).unwrap();
        assert_eq!(request_summary["key"], "tab");
        assert_eq!(request_summary["modifiers"], json!(["shift"]));
        assert!(!request_serialized.contains("MUST_NOT_PERSIST"));
        assert!(request_summary.get("keycode").is_none());

        let output = json!({
            "platform": "macos",
            "surface_id": "surface_safe",
            "key": "tab",
            "modifiers": ["shift"],
            "success": true,
            "title": "PRIVATE FOCUSED TARGET",
            "value": "SUPER_SECRET_VALUE"
        });
        let summary = session_log_result_for_tool("computer_key_input", &output);
        let serialized = serde_json::to_string(&summary).unwrap();
        assert_eq!(summary["surface_id"], "surface_safe");
        assert_eq!(summary["key"], "tab");
        assert_eq!(summary["modifiers"], json!(["shift"]));
        assert_eq!(summary["success"], true);
        assert!(!serialized.contains("PRIVATE FOCUSED TARGET"));
        assert!(!serialized.contains("SUPER_SECRET_VALUE"));
    }

    #[test]
    fn computer_text_input_request_and_result_never_persist_text() {
        let secret = "不要记录我🙂";
        let request = json!({
            "client_id": "mini",
            "surface_id": "surface_safe",
            "element_id": "element_safe",
            "text": secret,
        });
        let request_summary =
            session_log_arguments_for_tool_request("computer_input_text", &request);
        let request_serialized = serde_json::to_string(&request_summary).unwrap();
        assert_eq!(request_summary["client_id"], "mini");
        assert_eq!(request_summary["surface_id"], "surface_safe");
        assert_eq!(request_summary["element_id"], "element_safe");
        assert_eq!(request_summary["text_bytes"], secret.len());
        assert!(!request_serialized.contains(secret));
        assert!(request_summary.get("text").is_none());

        let typed = ToolCall::ComputerInputText {
            client_id: "mini".to_string(),
            surface_id: "surface_safe".to_string(),
            element_id: "element_safe".to_string(),
            text: secret.to_string(),
        };
        let typed_summary = typed.session_log_arguments();
        let typed_serialized = serde_json::to_string(&typed_summary).unwrap();
        assert_eq!(typed_summary["text_bytes"], secret.len());
        assert!(!typed_serialized.contains(secret));
        assert!(typed_summary.get("text").is_none());

        let output = json!({
            "platform": "macos",
            "surface_id": "surface_safe",
            "element_id": "element_safe",
            "text_bytes": secret.len(),
            "success": true,
            "text": secret,
            "value": secret,
        });
        let result_summary = session_log_result_for_tool("computer_input_text", &output);
        let result_serialized = serde_json::to_string(&result_summary).unwrap();
        assert_eq!(result_summary["text_bytes"], secret.len());
        assert_eq!(result_summary["success"], true);
        assert!(!result_serialized.contains(secret));
        assert!(result_summary.get("text").is_none());
        assert!(result_summary.get("value").is_none());
    }

    #[test]
    fn computer_snapshot_ledger_request_omits_region_coordinates() {
        let request = json!({
            "client_id": "mini",
            "surface_id": "surface_safe",
            "region": {"x": 111, "y": 222, "width": 333, "height": 444},
            "max_width": 800,
            "max_height": 600
        });
        let summary = session_log_arguments_for_tool_request("computer_snapshot", &request);
        let serialized = serde_json::to_string(&summary).unwrap();
        assert_eq!(summary["region_present"], true);
        assert_eq!(summary["max_width"], 800);
        assert_eq!(summary["max_height"], 600);
        assert!(summary.get("region").is_none());
        assert!(!serialized.contains("111"));
        assert!(!serialized.contains("222"));
        assert!(!serialized.contains("333"));
        assert!(!serialized.contains("444"));
    }

    #[test]
    fn computer_snapshot_ledger_result_omits_image_and_titles() {
        // Snapshot privacy remains unchanged.
        let output = json!({
            "surface": {
                "surface_id": "surface_safe",
                "application": "Private App",
                "title": "Confidential Window Title",
                "width": 1200,
                "height": 800,
                "focused": null,
                "active": null
            },
            "source_width": 1200,
            "source_height": 800,
            "region": {"x": 111, "y": 222, "width": 900, "height": 600},
            "width": 900,
            "height": 600,
            "mime_type": "image/jpeg",
            "file_bytes": 12345,
            "sha256": "PRIVATE_SCREENSHOT_DIGEST",
            "captured_at_unix_ms": 1700000000000u64,
            "content_base64": "SUPER_SECRET_SCREENSHOT_BYTES"
        });
        let summary = session_log_result_for_tool("computer_snapshot", &output);
        let serialized = serde_json::to_string(&summary).unwrap();
        assert_eq!(summary["surface_id"], "surface_safe");
        assert_eq!(summary["width"], 900);
        assert_eq!(summary["height"], 600);
        assert_eq!(summary["file_bytes"], 12345);
        assert_eq!(summary["region_present"], true);
        assert!(summary.get("sha256").is_none());
        assert!(summary.get("region").is_none());
        assert!(!serialized.contains("SUPER_SECRET"));
        assert!(!serialized.contains("Confidential"));
        assert!(!serialized.contains("Private App"));
    }

    #[test]
    fn computer_save_snapshot_audit_omits_image_digest_region_coordinates_and_session() {
        let request = json!({
            "project": "agent:target:demo",
            "path": "artifacts/ui.jpg",
            "client_id": "source-mac",
            "surface_id": "surface_safe",
            "region": {"x": 111, "y": 222, "width": 333, "height": 444},
            "max_width": 800,
            "max_height": 600,
            "session_id": "wc_sess_private"
        });
        let request_summary =
            session_log_arguments_for_tool_request("computer_save_snapshot", &request);
        let request_serialized = serde_json::to_string(&request_summary).unwrap();
        assert_eq!(request_summary["project"], "agent:target:demo");
        assert_eq!(request_summary["path"], "artifacts/ui.jpg");
        assert_eq!(request_summary["region_present"], true);
        assert!(request_summary.get("region").is_none());
        assert!(request_summary.get("session_id").is_none());
        for secret in ["111", "222", "333", "444", "wc_sess_private"] {
            assert!(!request_serialized.contains(secret));
        }

        let parsed_summary = ToolCall::ComputerSaveSnapshot {
            project: "agent:target:demo".to_string(),
            path: "artifacts/ui.jpg".to_string(),
            client_id: "source-mac".to_string(),
            surface_id: "surface_safe".to_string(),
            region: Some(ComputerSnapshotRegion {
                x: 111,
                y: 222,
                width: 333,
                height: 444,
            }),
            max_width: Some(800),
            max_height: Some(600),
            session_id: Some("wc_sess_private".to_string()),
        }
        .session_log_arguments();
        let parsed_serialized = serde_json::to_string(&parsed_summary).unwrap();
        assert_eq!(parsed_summary["region_present"], true);
        assert!(parsed_summary.get("region").is_none());
        assert!(parsed_summary.get("session_id").is_none());
        for secret in ["111", "222", "333", "444", "wc_sess_private"] {
            assert!(!parsed_serialized.contains(secret));
        }

        let output = json!({
            "project": "agent:target:demo",
            "path": "artifacts/ui.jpg",
            "client_id": "source-mac",
            "surface_id": "surface_safe",
            "source_width": 1200,
            "source_height": 800,
            "region": {"x": 111, "y": 222, "width": 900, "height": 600},
            "width": 900,
            "height": 600,
            "mime_type": "image/jpeg",
            "file_bytes": 12345,
            "sha256": "PRIVATE_SCREENSHOT_DIGEST",
            "saved": true,
            "content_base64": "SUPER_SECRET_SCREENSHOT_BYTES",
            "surface": {"application": "Private App", "title": "Confidential"}
        });
        let result_summary = session_log_result_for_tool("computer_save_snapshot", &output);
        let result_serialized = serde_json::to_string(&result_summary).unwrap();
        assert_eq!(result_summary["saved"], true);
        assert_eq!(result_summary["file_bytes"], 12345);
        assert_eq!(result_summary["region_present"], true);
        assert!(result_summary.get("sha256").is_none());
        assert!(result_summary.get("region").is_none());
        assert!(!result_serialized.contains("PRIVATE_SCREENSHOT_DIGEST"));
        assert!(!result_serialized.contains("SUPER_SECRET"));
        assert!(!result_serialized.contains("Private App"));
        assert!(!result_serialized.contains("Confidential"));
    }
    #[test]
    fn coding_agent_audit_is_body_free_for_requests_and_observations() {
        const PROMPT: &str = "PRIVATE_ACP_PROMPT_DO_NOT_PERSIST";
        const IDEMPOTENCY: &str = "PRIVATE_ACP_IDEMPOTENCY_KEY";
        const MESSAGE: &str = "PRIVATE_AGENT_MESSAGE_BODY";
        const REASONING: &str = "PRIVATE_REASONING_BODY";
        const TOOL_LABEL: &str = "PRIVATE_TOOL_LABEL";
        const TOKEN: &str = "PRIVATE_OBSERVATION_TOKEN";

        let request = json!({
            "project": "agent:special:demo",
            "provider_id": "codex",
            "idempotency_key": IDEMPOTENCY,
            "instruction": PROMPT,
            "config": {"mode": "agent"},
            "timeout_secs": 60,
            "recording_session_id": "wc_sess_safe"
        });
        let request_summary =
            session_log_arguments_for_tool_request("coding_agent_start", &request);
        let request_serialized = serde_json::to_string(&request_summary).unwrap();
        assert_eq!(request_summary["instruction_bytes"], PROMPT.len());
        assert_eq!(request_summary["config_count"], 1);
        assert_eq!(request_summary["idempotency_key_present"], true);
        assert!(!request_serialized.contains(PROMPT));
        assert!(!request_serialized.contains(IDEMPOTENCY));
        assert!(!request_serialized.contains("agent\""));
        assert!(request_summary.get("recording_session_id").is_none());
        assert!(!request_serialized.contains("wc_sess_safe"));

        let observe_request = json!({
            "run_id": "wc_agent_run_safe",
            "after_observation_token": TOKEN,
            "wait_secs": 3
        });
        let observe_request_summary =
            session_log_arguments_for_tool_request("coding_agent_observe", &observe_request);
        let observe_request_serialized = serde_json::to_string(&observe_request_summary).unwrap();
        assert_eq!(observe_request_summary["token_present"], true);
        assert!(!observe_request_serialized.contains(TOKEN));

        let output = json!({
            "run_id": "wc_agent_run_safe",
            "project": "agent:special:demo",
            "provider_id": "codex",
            "state": "running",
            "execution_state": "started",
            "events": [
                {"sequence": 1, "kind": "agent_message", "text": MESSAGE, "label": null, "status": null, "usage": null},
                {"sequence": 2, "kind": "reasoning", "text": REASONING, "label": null, "status": null, "usage": null},
                {"sequence": 3, "kind": "tool_activity", "text": null, "label": TOOL_LABEL, "status": "running", "usage": null}
            ],
            "observation_token": TOKEN,
            "has_more": false,
            "history_lost": false,
            "first_retained_sequence": 1,
            "terminal": null,
            "recovery_kind": "reobserve"
        });
        let result_summary = session_log_result_for_tool("coding_agent_observe", &output);
        let result_serialized = serde_json::to_string(&result_summary).unwrap();
        assert_eq!(result_summary["event_count"], 3);
        assert_eq!(
            result_summary["event_body_bytes"],
            MESSAGE.len() + REASONING.len()
        );
        for private in [MESSAGE, REASONING, TOOL_LABEL, TOKEN] {
            assert!(!result_serialized.contains(private));
        }
        assert!(result_summary.get("events").is_none());
        assert!(result_summary.get("observation_token").is_none());
    }
}

fn project_serialized_call(
    policy: Option<&ToolAuditPolicy>,
    serialized: Result<Value, serde_json::Error>,
) -> Value {
    let Ok(Value::Object(serialized)) = serialized else {
        return Value::Null;
    };
    let empty = serde_json::json!({});
    let arguments = serialized.get("params").unwrap_or(&empty);
    project_arguments(policy, arguments, AuditStage::Typed)
}

impl ToolCall {
    pub fn session_log_arguments(&self) -> Value {
        // Use the canonical accessor, not a second match over ToolCall variants.
        // Serialization is ephemeral and never replaces execution arguments.
        // A missing definition or serialization error has no raw fallback.
        project_serialized_call(
            lookup_tool_definition(self.tool_name()).map(|definition| &definition.audit),
            serde_json::to_value(self),
        )
    }
}

#[cfg(test)]
#[path = "tool_audit_contract_tests.rs"]
mod contract_tests;
