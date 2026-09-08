//! Final bounded Workflow Session audit projection driven by canonical ToolDefinition policy.
//!
//! Runtime request/result auditing remains upstream. This layer owns only the final
//! persisted Session input/context/execution privacy fence and restore-time re-projection.

use serde_json::{json, Value};
use webcodex_tool_contracts::{
    lookup_tool_definition, ToolAuditContextPolicy, ToolAuditExecutionPolicy, ToolAuditResultField,
    ToolAuditResultPolicy, ToolAuditSessionInputPolicy,
};

use super::util::{redact_and_bound_value, validation_excerpt};

pub(super) fn audit_policy_for_tool(
    name: &str,
) -> Option<webcodex_tool_contracts::ToolAuditPolicy> {
    lookup_tool_definition(name).map(|definition| definition.audit_policy())
}

/// Build the final bounded input retained on a Session event. The ordinary
/// runtime supplies an already-audited typed projection; the per-definition
/// Session policy preserves the historical defense-in-depth filters for direct
/// internal SessionStore callers without maintaining another tool-name registry.
pub fn session_input_summary_for_tool(tool_name: &str, arguments: &Value) -> Value {
    // Historical pre-0.4 Session ledgers may still contain this alias. It shares
    // the canonical list_runners policy but never becomes a runtime audit identity.
    let canonical_name = if tool_name == "list_agents" {
        "list_runners"
    } else {
        tool_name
    };
    let policy = match audit_policy_for_tool(canonical_name) {
        Some(policy) => policy.session_input,
        None if tool_name == "start_coding_task" => {
            // Retired wire rejection compatibility only. The runtime rejection
            // projector already emits a bounded body-free summary; keep the same
            // final generic redaction while unknown open-world names fail closed.
            return redact_and_bound_value(arguments);
        }
        None => return json!({}),
    };

    let mut summary = redact_and_bound_value(arguments);
    let Some(object) = summary.as_object_mut() else {
        return json!({});
    };
    match policy {
        ToolAuditSessionInputPolicy::Bounded => {}
        ToolAuditSessionInputPolicy::OmitTopLevel(fields) => {
            for field in fields {
                object.remove(*field);
            }
        }
        ToolAuditSessionInputPolicy::SearchProjectTexts => {
            if let Some(queries) = object.get_mut("queries").and_then(Value::as_array_mut) {
                for query in queries.iter_mut().filter_map(Value::as_object_mut) {
                    query.remove("pattern");
                }
            }
        }
        ToolAuditSessionInputPolicy::ObserveJobs => {
            if let Some(items) = object.get_mut("items").and_then(Value::as_array_mut) {
                for item in items.iter_mut().filter_map(Value::as_object_mut) {
                    item.remove("after_observation_token");
                }
            }
        }
    }
    summary
}

fn project_context_fields(fields: &[ToolAuditResultField], output: &Value) -> Option<Value> {
    let mut summary = serde_json::Map::new();
    for field in fields {
        let (name, projected) = match *field {
            ToolAuditResultField::Value {
                output: name,
                source,
            } => (name, output.get(source).cloned()),
            ToolAuditResultField::Pointer {
                output: name,
                pointer,
            } => (name, output.pointer(pointer).cloned()),
            ToolAuditResultField::ArrayLen {
                output: name,
                source,
            } => (
                name,
                output
                    .get(source)
                    .and_then(Value::as_array)
                    .map(|value| json!(value.len())),
            ),
            ToolAuditResultField::PointerArrayLen {
                output: name,
                pointer,
            } => (
                name,
                output
                    .pointer(pointer)
                    .and_then(Value::as_array)
                    .map(|value| json!(value.len())),
            ),
            ToolAuditResultField::StringBytes {
                output: name,
                source,
            } => (
                name,
                output
                    .get(source)
                    .and_then(Value::as_str)
                    .map(|value| json!(value.len())),
            ),
            ToolAuditResultField::Presence {
                output: name,
                source,
            } => (
                name,
                Some(json!(output
                    .get(source)
                    .is_some_and(|value| !value.is_null()))),
            ),
            ToolAuditResultField::StringPresent {
                output: name,
                source,
            } => (
                name,
                Some(json!(output.get(source).and_then(Value::as_str).is_some())),
            ),
            ToolAuditResultField::PointerNonNull {
                output: name,
                pointer,
            } => (
                name,
                Some(json!(output
                    .pointer(pointer)
                    .is_some_and(|value| !value.is_null()))),
            ),
        };
        if let Some(projected) = projected {
            summary.insert(name.to_string(), projected);
        }
    }
    (!summary.is_empty()).then(|| Value::Object(summary))
}

fn field_output_name(field: &ToolAuditResultField) -> &'static str {
    match *field {
        ToolAuditResultField::Value { output, .. }
        | ToolAuditResultField::Pointer { output, .. }
        | ToolAuditResultField::ArrayLen { output, .. }
        | ToolAuditResultField::PointerArrayLen { output, .. }
        | ToolAuditResultField::StringBytes { output, .. }
        | ToolAuditResultField::Presence { output, .. }
        | ToolAuditResultField::StringPresent { output, .. }
        | ToolAuditResultField::PointerNonNull { output, .. } => output,
    }
}

fn project_already_audited_context_fields(
    fields: &[ToolAuditResultField],
    output: &Value,
) -> Option<Value> {
    let source = output.as_object()?;
    let mut summary = serde_json::Map::new();
    for field in fields {
        let name = field_output_name(field);
        if let Some(value) = source.get(name) {
            summary.insert(name.to_string(), value.clone());
        }
    }
    (!summary.is_empty()).then(|| Value::Object(summary))
}

pub(super) fn context_result_summary_for_tool_result(
    tool_name: &str,
    output: &Value,
) -> Option<Value> {
    let policy = audit_policy_for_tool(tool_name)?;
    let summary = match policy.context {
        ToolAuditContextPolicy::Omit => return None,
        ToolAuditContextPolicy::ResultProjection => match policy.result {
            // Runtime result auditing has already flattened pointer/derived fields
            // to each declaration's output name before Session recording. Reuse
            // that audited shape instead of traversing the original raw sources a
            // second time.
            ToolAuditResultPolicy::Fields(fields) => {
                project_already_audited_context_fields(fields, output)
            }
            // Context reuse is intentionally invalid for canonical/raw evidence or
            // semantic result projectors; declaration tests prevent this shape.
            ToolAuditResultPolicy::CanonicalLedgerEvidence | ToolAuditResultPolicy::Semantic(_) => {
                None
            }
        },
        ToolAuditContextPolicy::Fields(fields) => project_context_fields(fields, output),
        ToolAuditContextPolicy::WorkingTreeStatus => {
            let source = output.as_object()?;
            if source.contains_key("status_excerpt") {
                let mut summary = serde_json::Map::new();
                for key in ["clean", "status_excerpt", "status_truncated", "exit_code"] {
                    if let Some(value) = source.get(key) {
                        summary.insert(key.to_string(), value.clone());
                    }
                }
                (!summary.is_empty()).then(|| Value::Object(summary))
            } else {
                let stdout = source
                    .get("stdout")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let excerpt = validation_excerpt(stdout);
                Some(json!({
                    "clean": stdout.trim().is_empty(),
                    "status_excerpt": excerpt.text,
                    "status_truncated": excerpt.filtered,
                    "exit_code": source.get("exit_code").cloned().unwrap_or(Value::Null),
                }))
            }
        }
    }?;
    Some(redact_and_bound_value(&summary))
}

pub(super) fn execution_policy_for_tool(tool_name: &str) -> Option<ToolAuditExecutionPolicy> {
    audit_policy_for_tool(tool_name)
        .map(|policy| policy.execution)
        .filter(|policy| policy.detail != webcodex_tool_contracts::ToolAuditExecutionDetail::Omit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_session_audit_identity_fails_closed_but_retired_alias_reuses_canonical_policy() {
        let raw = json!({
            "client_id": "PRIVATE_RUNNER",
            "client_ids": ["PRIVATE_RUNNER"],
            "summary_only": true
        });
        assert_eq!(session_input_summary_for_tool("unknown", &raw), json!({}));
        assert_eq!(
            session_input_summary_for_tool("list_agents", &raw),
            session_input_summary_for_tool("list_runners", &raw)
        );
        assert!(!session_input_summary_for_tool("list_agents", &raw)
            .to_string()
            .contains("PRIVATE_"));
    }

    #[test]
    fn direct_session_store_execution_inputs_keep_the_existing_body_free_fence() {
        let process = session_input_summary_for_tool(
            "run_process",
            &json!({
                "project":"demo",
                "executable":"PRIVATE_EXECUTABLE",
                "args":["PRIVATE_ARG"],
                "stdin":"PRIVATE_STDIN",
                "process_summary":"PRIVATE_PREVIEW",
                "arg_count":1,
                "stdin_present":true
            }),
        );
        assert_eq!(process["arg_count"], 1);
        assert_eq!(process["stdin_present"], true);
        assert!(!process.to_string().contains("PRIVATE_"));
    }

    #[test]
    fn result_projection_reuse_and_working_tree_semantics_are_definition_owned() {
        let agent = context_result_summary_for_tool_result(
            "create_agent_identity",
            &json!({
                "agent_id":"wc_dagent_demo","profile_revision":3,
                "created":true,"replayed":false,"state_changed":true
            }),
        )
        .unwrap();
        assert_eq!(agent["agent_id"], "wc_dagent_demo");
        let status = context_result_summary_for_tool_result(
            "git_status",
            &json!({"stdout":" M src/lib.rs\n", "exit_code":0}),
        )
        .unwrap();
        assert_eq!(status["clean"], false);
        assert!(status["status_excerpt"]
            .as_str()
            .unwrap()
            .contains("src/lib.rs"));
    }
}
