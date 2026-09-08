//! Final persisted projection of canonical audit policy. No tool registry lives here.
use serde_json::{json, Value};
use webcodex_tool_contracts::{audit_policy::*, lookup_tool_definition};

use super::util::{redact_and_bound_value, validation_excerpt};

pub(super) fn audit_policy_for_tool(name: &str) -> Option<&'static ToolAuditPolicy> {
    lookup_tool_definition(name)
        .map(|definition| &definition.audit)
        .filter(|policy| policy.is_valid())
}

pub fn session_input_summary_for_tool(tool_name: &str, arguments: &Value) -> Value {
    // Restore compatibility for the historical Runner-list spelling only. New
    // runtime audit lookup does not accept aliases; this shares canonical policy.
    let canonical_name = if tool_name == "list_agents" {
        "list_runners"
    } else {
        tool_name
    };
    session_input_summary_for_policy(audit_policy_for_tool(canonical_name), arguments)
}

fn session_input_summary_for_policy(policy: Option<&ToolAuditPolicy>, arguments: &Value) -> Value {
    let Some(policy) = policy.filter(|policy| policy.is_valid()) else {
        return Value::Null;
    };
    // Preserve the existing order of bounds and privacy filtering, including
    // truncation evidence. Projection is not used as an execution input.
    let mut summary = redact_and_bound_value(arguments);
    let Some(object) = summary.as_object_mut() else {
        return Value::Null;
    };
    let typed_fields = match policy.request.typed {
        AuditTypedPolicy::Overrides { fields, .. } => fields,
        AuditTypedPolicy::Same | AuditTypedPolicy::Omit => &[],
    };
    let fields: Vec<_> = policy.request.fields.iter().chain(typed_fields).collect();
    // Legacy structured-validation events may carry scoped selectors instead
    // of a normalized target id. The evidence reducer still needs their existing
    // bounded metadata to correlate outcomes. Canonical runtime projection emits
    // the target id and omits those selectors before reaching this boundary.
    let legacy_validation_selectors = !object.contains_key("validation_target_id")
        && fields
            .iter()
            .any(|field| field.value == AuditValue::ValidationIdentity);
    for field in &fields {
        if field.value == AuditValue::EphemeralPreview {
            object.remove(field.destination);
        }
        // Batch query metadata is retained, but never its search pattern bodies.
        // The Count rule for the same source must not discard this legacy shape.
        if fields
            .iter()
            .any(|rule| rule.source == field.source && rule.value == AuditValue::AnyPattern)
        {
            if let Some(queries) = object.get_mut(field.source).and_then(Value::as_array_mut) {
                for query in queries {
                    if let Some(query) = query.as_object_mut() {
                        query.remove("pattern");
                    }
                }
            }
            continue;
        }
        // A summarized private source is not another ledger field. Do not
        // remove an independently declared copied metadata field, or a value
        // whose projection intentionally keeps the same key (e.g. context).
        let retained = fields.iter().any(|rule| rule.destination == field.source);
        if field.source != field.destination
            && !retained
            && !legacy_validation_selectors
            && !matches!(
                field.value,
                AuditValue::Copy | AuditValue::Nullable | AuditValue::ValidationIdentity
            )
        {
            object.remove(field.source);
        }
    }
    let omitted: &[&str] = match policy.request.transform {
        AuditTransform::ProcessExecution => &["executable", "args", "stdin", "process_summary"],
        AuditTransform::DetachedExecution => &[
            "executable",
            "args",
            "stdin",
            "process_summary",
            "idempotency_key",
        ],
        AuditTransform::ScriptExecution => &["script", "args", "stdin", "script_summary"],
        AuditTransform::Checkpoint => &["note", "labels", "validation"],
        AuditTransform::Edits => &["changes"],
        AuditTransform::Fields | AuditTransform::JobObservation => &[],
    };
    for key in omitted {
        object.remove(*key);
    }
    if policy.request.transform == AuditTransform::JobObservation {
        if let Some(items) = object.get_mut("items").and_then(Value::as_array_mut) {
            for item in items {
                if let Some(item) = item.as_object_mut() {
                    item.remove("after_observation_token");
                }
            }
        }
    }
    summary
}

pub(super) fn context_result_summary_for_tool_result(
    tool_name: &str,
    output: &Value,
) -> Option<Value> {
    context_result_summary_for_policy(audit_policy_for_tool(tool_name), output)
}

fn context_result_summary_for_policy(
    policy: Option<&ToolAuditPolicy>,
    output: &Value,
) -> Option<Value> {
    let policy = policy.filter(|policy| policy.is_valid())?;
    let source = output.as_object()?;
    let mut summary = serde_json::Map::new();
    match policy.context {
        AuditContextPolicy::Omit => return None,
        AuditContextPolicy::Fields(fields) => {
            for field in fields {
                let value = if field.source.starts_with('/') {
                    output.pointer(field.source)
                } else {
                    source.get(field.source)
                };
                let value = match field.value {
                    AuditValue::Copy => {
                        let Some(value) = value else {
                            continue;
                        };
                        value.clone()
                    }
                    AuditValue::Nullable => value.cloned().unwrap_or(Value::Null),
                    AuditValue::NullableCount => value
                        .and_then(Value::as_array)
                        .map(|value| json!(value.len()))
                        .unwrap_or(Value::Null),
                    AuditValue::NullableBytes => value
                        .and_then(Value::as_str)
                        .map(|value| json!(value.len()))
                        .unwrap_or(Value::Null),
                    // Context declarations deliberately support only these
                    // bounded reducers; malformed policies cannot pass through.
                    _ => return None,
                };
                summary.insert(field.destination.to_owned(), value);
            }
        }
        AuditContextPolicy::WorkingTreeStatus => {
            if source.contains_key("status_excerpt") {
                for key in ["clean", "status_excerpt", "status_truncated", "exit_code"] {
                    if let Some(value) = source.get(key) {
                        summary.insert(key.to_owned(), value.clone());
                    }
                }
            } else {
                let stdout = source
                    .get("stdout")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let excerpt = validation_excerpt(stdout);
                summary.insert("clean".into(), json!(stdout.trim().is_empty()));
                summary.insert("status_excerpt".into(), json!(excerpt.text));
                summary.insert("status_truncated".into(), json!(excerpt.filtered));
                summary.insert(
                    "exit_code".into(),
                    source.get("exit_code").cloned().unwrap_or(Value::Null),
                );
            }
        }
    }
    (!summary.is_empty()).then(|| redact_and_bound_value(&Value::Object(summary)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_invalid_or_unknown_ledger_policy_is_closed() {
        let raw = json!({"body":"PRIVATE_BODY", "stdout":"PRIVATE_STDOUT"});
        assert_eq!(session_input_summary_for_tool("unknown", &raw), Value::Null);
        assert_eq!(session_input_summary_for_policy(None, &raw), Value::Null);
        assert_eq!(context_result_summary_for_policy(None, &raw), None);
        let mut invalid = *audit_policy_for_tool("run_process").unwrap();
        const BAD: &[AuditField] = &[AuditField::new("body", "body", AuditValue::Preview)];
        invalid.context = AuditContextPolicy::Fields(BAD);
        assert_eq!(
            session_input_summary_for_policy(Some(&invalid), &raw),
            Value::Null
        );
        assert_eq!(
            context_result_summary_for_policy(Some(&invalid), &raw),
            None
        );
    }

    #[test]
    fn semantic_execution_privacy_does_not_depend_on_tool_identity() {
        let policy = *audit_policy_for_tool("run_process").unwrap();
        // A new declaration can reuse this policy without adding a Session
        // name branch. The projector accepts policy, not a second tool identity.
        let raw = json!({"project":"demo", "executable":"PRIVATE_EXECUTABLE", "args":["PRIVATE_ARG"], "stdin":"PRIVATE_STDIN", "process_summary":"PRIVATE_PREVIEW", "arg_count":1, "stdin_present":true});
        let summary = session_input_summary_for_policy(Some(&policy), &raw);
        assert!(!summary.to_string().contains("PRIVATE_"));
        assert_eq!(summary["arg_count"], 1);
        assert_eq!(summary["stdin_present"], true);
        assert_eq!(raw["stdin"], "PRIVATE_STDIN");
    }

    #[test]
    fn ephemeral_preview_and_legacy_alias_share_canonical_privacy() {
        let raw = json!({"command":"PRIVATE_COMMAND", "command_summary":"PRIVATE_PREVIEW", "command_present":true});
        let summary = session_input_summary_for_policy(audit_policy_for_tool("run_shell"), &raw);
        assert_eq!(summary, json!({"command_present":true}));
        let legacy = json!({"client_id":"PRIVATE_RUNNER", "client_ids":["PRIVATE_RUNNER"], "summary_only":true});
        assert_eq!(
            session_input_summary_for_tool("list_agents", &legacy),
            session_input_summary_for_tool("list_runners", &legacy)
        );
        assert!(!session_input_summary_for_tool("list_agents", &legacy)
            .to_string()
            .contains("PRIVATE_"));
    }
}
