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

pub fn session_log_arguments_for_tool_request(tool_name: &str, arguments: &Value) -> Value {
    // This retired wire name is deliberately not a ToolDefinition. The kernel
    // records a bounded rejection summary before concrete ToolCall parsing, so
    // keep one compatibility-only sanitizer without reviving it as tool identity.
    if tool_name == "start_coding_task" {
        return retired_start_coding_task_audit(arguments);
    }

    let Some(definition) = webcodex_tool_contracts::lookup_tool_definition(tool_name) else {
        return empty_audit_projection();
    };
    let request_policy = definition.audit_policy().request;
    if !matches!(
        request_policy,
        webcodex_tool_contracts::ToolAuditRequestPolicy::Typed
            | webcodex_tool_contracts::ToolAuditRequestPolicy::TypedDropNullValues
    ) {
        return empty_audit_projection();
    }

    let Some(call) = audit_tool_call_from_request(definition, tool_name, arguments) else {
        return empty_audit_projection();
    };
    let mut projected = call.session_log_arguments();
    if request_policy == webcodex_tool_contracts::ToolAuditRequestPolicy::TypedDropNullValues {
        if let Some(projected) = projected.as_object_mut() {
            projected.retain(|_, value| !value.is_null());
        }
    }
    projected
}

fn empty_audit_projection() -> Value {
    serde_json::json!({})
}

fn audit_tool_call_from_request(
    definition: &webcodex_tool_contracts::ToolDefinition,
    tool_name: &str,
    arguments: &Value,
) -> Option<ToolCall> {
    if let Ok(call) = ToolCall::from_tool_name(tool_name, arguments.clone()) {
        return Some(call);
    }

    // Audit happens before the authoritative concrete parser so pre-execution
    // denials can still be recorded. For model-visible tools, retry after
    // dropping undeclared top-level fields using the canonical ToolDefinition
    // schema. This preserves existing privacy summaries for malformed requests
    // without ever persisting the unknown fields themselves.
    let schema = definition.model_spec?.input_schema;
    let schema = schema();
    let allowed = schema.get("properties")?.as_object()?;
    let source = arguments.as_object()?;
    let filtered = source
        .iter()
        .filter(|(key, _)| allowed.contains_key(*key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    if filtered.len() == source.len() {
        return None;
    }
    ToolCall::from_tool_name(tool_name, Value::Object(filtered)).ok()
}

fn retired_start_coding_task_audit(arguments: &Value) -> Value {
    let Some(obj) = arguments.as_object() else {
        return empty_audit_projection();
    };
    let mut out = serde_json::Map::new();
    copy_keys(
        obj,
        &mut out,
        &[
            "project",
            "client_id",
            "title",
            "mode",
            "deny_write_tools",
            "deny_shell_tools",
            "detail",
            "resume_session_id",
            "session_id",
        ],
    );
    out.insert(
        "path_source_requested".to_string(),
        Value::Bool(obj.contains_key("path")),
    );
    let context = obj
        .get("execution_context")
        .cloned()
        .and_then(|value| serde_json::from_value::<SessionExecutionContext>(value).ok())
        .map(|context| context.audit_summary())
        .unwrap_or(Value::Null);
    out.insert("execution_context".to_string(), context);
    Value::Object(out)
}

#[derive(Debug, Clone, Copy)]
enum StructuredValidationRequestAudit {
    CargoFmt,
    CargoCheck,
    CargoTest,
    GoTest,
}

impl StructuredValidationRequestAudit {
    fn tool_name(self) -> &'static str {
        match self {
            Self::CargoFmt => "cargo_fmt",
            Self::CargoCheck => "cargo_check",
            Self::CargoTest => "cargo_test",
            Self::GoTest => "go_test",
        }
    }
}

fn typed_structured_validation_request_audit(
    kind: StructuredValidationRequestAudit,
    arguments: &Value,
) -> Value {
    let Some(obj) = arguments.as_object() else {
        return empty_audit_projection();
    };
    let mut out = serde_json::Map::new();
    if let Some(project) = obj.get("project").cloned() {
        out.insert("project".to_string(), project);
    }
    match kind {
        StructuredValidationRequestAudit::CargoFmt => {
            copy_keys(
                obj,
                &mut out,
                &["cwd", "check", "timeout_secs", "sync_wait_secs"],
            );
            insert_structured_validation_target(kind.tool_name(), obj, &mut out);
        }
        StructuredValidationRequestAudit::CargoCheck => {
            copy_keys(
                obj,
                &mut out,
                &[
                    "cwd",
                    "all_targets",
                    "all_features",
                    "no_default_features",
                    "package",
                    "timeout_secs",
                    "sync_wait_secs",
                ],
            );
            out.insert(
                "features_present".to_string(),
                Value::Bool(
                    obj.get("features")
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.is_empty()),
                ),
            );
            insert_structured_validation_target(kind.tool_name(), obj, &mut out);
        }
        StructuredValidationRequestAudit::CargoTest => {
            copy_keys(
                obj,
                &mut out,
                &[
                    "cwd",
                    "all_targets",
                    "all_features",
                    "no_default_features",
                    "package",
                    "no_run",
                    "require_tests",
                    "min_tests",
                    "timeout_secs",
                    "sync_wait_secs",
                ],
            );
            out.insert(
                "filter_present".to_string(),
                Value::Bool(
                    obj.get("filter")
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.is_empty()),
                ),
            );
            out.insert(
                "features_present".to_string(),
                Value::Bool(
                    obj.get("features")
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.is_empty()),
                ),
            );
            insert_structured_validation_target(kind.tool_name(), obj, &mut out);
        }
        StructuredValidationRequestAudit::GoTest => {
            copy_keys(obj, &mut out, &["cwd", "timeout_secs", "sync_wait_secs"]);
            let packages = obj.get("packages").and_then(Value::as_array);
            out.insert(
                "packages_present".to_string(),
                Value::Bool(obj.get("packages").is_some_and(|value| !value.is_null())),
            );
            out.insert(
                "package_count".to_string(),
                Value::from(packages.map(Vec::len).unwrap_or_default()),
            );
            insert_structured_validation_target(kind.tool_name(), obj, &mut out);
        }
    }
    Value::Object(out)
}

#[derive(Debug, Clone, Copy)]
enum AgentTaskRequestAudit {
    Create,
    List,
    Read,
    Assign,
    StartAttempt,
    StartCodingRun,
    ReconcileCodingRun,
    HeartbeatAttempt,
    CompleteAttempt,
}

fn typed_agent_task_request_audit(kind: AgentTaskRequestAudit, arguments: &Value) -> Value {
    let Some(obj) = arguments.as_object() else {
        return empty_audit_projection();
    };
    let mut out = serde_json::Map::new();
    if let Some(project) = obj.get("project").cloned() {
        out.insert("project".to_string(), project);
    }
    match kind {
        AgentTaskRequestAudit::Create => {
            copy_keys(
                obj,
                &mut out,
                &[
                    "assignee_agent_id",
                    "source_conversation_id",
                    "source_message_id",
                    "referenced_project_id",
                ],
            );
            out.insert(
                "title_chars".to_string(),
                Value::from(
                    obj.get("title")
                        .and_then(Value::as_str)
                        .map(str::chars)
                        .map(Iterator::count)
                        .unwrap_or_default(),
                ),
            );
            out.insert(
                "instruction_bytes".to_string(),
                Value::from(
                    obj.get("instruction")
                        .and_then(Value::as_str)
                        .map(str::len)
                        .unwrap_or_default(),
                ),
            );
            out.insert(
                "idempotency_key_present".to_string(),
                Value::Bool(obj.get("idempotency_key").and_then(Value::as_str).is_some()),
            );
        }
        AgentTaskRequestAudit::List => {
            copy_keys(obj, &mut out, &["assignee_agent_id", "offset", "limit"]);
        }
        AgentTaskRequestAudit::Read => {
            copy_keys(obj, &mut out, &["task_id"]);
        }
        AgentTaskRequestAudit::Assign => {
            copy_keys(obj, &mut out, &["task_id", "assignee_agent_id"]);
        }
        AgentTaskRequestAudit::StartAttempt => {
            copy_keys(obj, &mut out, &["task_id", "assignee_agent_id"]);
            out.insert(
                "idempotency_key_present".to_string(),
                Value::Bool(obj.get("idempotency_key").and_then(Value::as_str).is_some()),
            );
        }
        AgentTaskRequestAudit::StartCodingRun => {
            copy_keys(
                obj,
                &mut out,
                &[
                    "project",
                    "task_id",
                    "attempt_id",
                    "assignee_agent_id",
                    "attempt_controller_generation",
                    "provider_id",
                    "timeout_secs",
                ],
            );
            out.insert(
                "attempt_fence_present".to_string(),
                Value::Bool(obj.get("attempt_fence").and_then(Value::as_str).is_some()),
            );
            out.insert(
                "config_count".to_string(),
                Value::from(
                    obj.get("config")
                        .and_then(Value::as_object)
                        .map(serde_json::Map::len)
                        .unwrap_or_default(),
                ),
            );
        }
        AgentTaskRequestAudit::ReconcileCodingRun => {
            copy_keys(obj, &mut out, &["task_id", "attempt_id"]);
        }
        AgentTaskRequestAudit::HeartbeatAttempt => {
            copy_keys(
                obj,
                &mut out,
                &[
                    "task_id",
                    "attempt_id",
                    "assignee_agent_id",
                    "attempt_controller_generation",
                ],
            );
            out.insert(
                "attempt_fence_present".to_string(),
                Value::Bool(obj.get("attempt_fence").and_then(Value::as_str).is_some()),
            );
        }
        AgentTaskRequestAudit::CompleteAttempt => {
            copy_keys(
                obj,
                &mut out,
                &[
                    "task_id",
                    "attempt_id",
                    "assignee_agent_id",
                    "attempt_controller_generation",
                    "outcome",
                ],
            );
            out.insert(
                "attempt_fence_present".to_string(),
                Value::Bool(obj.get("attempt_fence").and_then(Value::as_str).is_some()),
            );
            out.insert(
                "terminal_result_bytes".to_string(),
                Value::from(
                    obj.get("terminal_result")
                        .and_then(Value::as_str)
                        .map(str::len)
                        .unwrap_or_default(),
                ),
            );
            out.insert(
                "terminal_reason_bytes".to_string(),
                Value::from(
                    obj.get("terminal_reason")
                        .and_then(Value::as_str)
                        .map(str::len)
                        .unwrap_or_default(),
                ),
            );
            out.insert(
                "completion_key_present".to_string(),
                Value::Bool(obj.get("completion_key").and_then(Value::as_str).is_some()),
            );
        }
    }
    Value::Object(out)
}

#[derive(Debug, Clone, Copy)]
enum CommunicationRequestAudit {
    CreateIdentity,
    ListIdentities,
    UpdateIdentity,
    AttachEndpoint,
    DetachEndpoint,
    CreateConversation,
    ListConversations,
    ReadConversation,
    PostMessage,
    ListInbox,
    ConsumeDeliveries,
    BootstrapConversation,
    ConsumeWake,
}

fn typed_communication_request_audit(kind: CommunicationRequestAudit, arguments: &Value) -> Value {
    let Some(obj) = arguments.as_object() else {
        return empty_audit_projection();
    };
    let mut out = serde_json::Map::new();
    if let Some(project) = obj.get("project").cloned() {
        out.insert("project".to_string(), project);
    }
    match kind {
        CommunicationRequestAudit::CreateIdentity => {
            out.insert(
                "handle_chars".to_string(),
                Value::from(
                    obj.get("handle")
                        .and_then(Value::as_str)
                        .map(str::chars)
                        .map(Iterator::count)
                        .unwrap_or_default(),
                ),
            );
            out.insert(
                "display_name_chars".to_string(),
                Value::from(
                    obj.get("display_name")
                        .and_then(Value::as_str)
                        .map(str::chars)
                        .map(Iterator::count)
                        .unwrap_or_default(),
                ),
            );
            out.insert(
                "description_bytes".to_string(),
                Value::from(
                    obj.get("description")
                        .and_then(Value::as_str)
                        .map(str::len)
                        .unwrap_or_default(),
                ),
            );
            out.insert(
                "specialty_label_count".to_string(),
                Value::from(
                    obj.get("specialty_labels")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or_default(),
                ),
            );
            out.insert(
                "idempotency_key_present".to_string(),
                Value::Bool(obj.get("idempotency_key").and_then(Value::as_str).is_some()),
            );
        }
        CommunicationRequestAudit::ListIdentities => {
            copy_keys(obj, &mut out, &["agent_id", "offset", "limit"]);
        }
        CommunicationRequestAudit::UpdateIdentity => {
            copy_keys(obj, &mut out, &["agent_id", "expected_profile_revision"]);
            for field in ["handle", "display_name", "description", "specialty_labels"] {
                out.insert(
                    format!("{field}_present"),
                    Value::Bool(obj.get(field).is_some_and(|value| !value.is_null())),
                );
            }
            out.insert(
                "description_bytes".to_string(),
                Value::from(
                    obj.get("description")
                        .and_then(Value::as_str)
                        .map(str::len)
                        .unwrap_or_default(),
                ),
            );
            out.insert(
                "specialty_label_count".to_string(),
                Value::from(
                    obj.get("specialty_labels")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or_default(),
                ),
            );
        }
        CommunicationRequestAudit::AttachEndpoint => {
            copy_keys(obj, &mut out, &["agent_id", "host"]);
            out.insert(
                "client_attachment_id_present".to_string(),
                Value::Bool(
                    obj.get("client_attachment_id")
                        .is_some_and(|value| !value.is_null()),
                ),
            );
            out.insert(
                "idempotency_key_present".to_string(),
                Value::Bool(obj.get("idempotency_key").and_then(Value::as_str).is_some()),
            );
        }
        CommunicationRequestAudit::DetachEndpoint => {
            copy_keys(obj, &mut out, &["endpoint_id"]);
        }
        CommunicationRequestAudit::CreateConversation => {
            out.insert(
                "title_present".to_string(),
                Value::Bool(obj.get("title").is_some_and(|value| !value.is_null())),
            );
            out.insert(
                "agent_count".to_string(),
                Value::from(
                    obj.get("agent_ids")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or_default(),
                ),
            );
            out.insert(
                "idempotency_key_present".to_string(),
                Value::Bool(obj.get("idempotency_key").and_then(Value::as_str).is_some()),
            );
        }
        CommunicationRequestAudit::ListConversations => {
            copy_keys(
                obj,
                &mut out,
                &[
                    "agent_id",
                    "endpoint_id",
                    "expected_controller_generation",
                    "offset",
                    "limit",
                ],
            );
        }
        CommunicationRequestAudit::ReadConversation => {
            copy_keys(
                obj,
                &mut out,
                &[
                    "conversation_id",
                    "agent_id",
                    "endpoint_id",
                    "expected_controller_generation",
                    "after_seq",
                    "limit",
                ],
            );
        }
        CommunicationRequestAudit::PostMessage => {
            copy_keys(
                obj,
                &mut out,
                &[
                    "conversation_id",
                    "author_agent_id",
                    "endpoint_id",
                    "expected_controller_generation",
                    "reply_to",
                    "wake_reply_id",
                    "reply_operation_index",
                ],
            );
            out.insert(
                "body_bytes".to_string(),
                Value::from(
                    obj.get("body")
                        .and_then(Value::as_str)
                        .map(str::len)
                        .unwrap_or_default(),
                ),
            );
            out.insert(
                "recipient_mode".to_string(),
                Value::String(
                    if obj.get("recipient_agent_ids").is_some_and(Value::is_array) {
                        "explicit".to_string()
                    } else {
                        "all_agents_except_author".to_string()
                    },
                ),
            );
            out.insert(
                "recipient_count".to_string(),
                Value::from(
                    obj.get("recipient_agent_ids")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or_default(),
                ),
            );
            out.insert(
                "idempotency_key_present".to_string(),
                Value::Bool(obj.get("idempotency_key").and_then(Value::as_str).is_some()),
            );
        }
        CommunicationRequestAudit::ListInbox => {
            copy_keys(
                obj,
                &mut out,
                &[
                    "agent_id",
                    "endpoint_id",
                    "expected_controller_generation",
                    "after_delivery_order",
                    "limit",
                ],
            );
        }
        CommunicationRequestAudit::ConsumeDeliveries => {
            copy_keys(
                obj,
                &mut out,
                &["agent_id", "endpoint_id", "expected_controller_generation"],
            );
            out.insert(
                "delivery_count".to_string(),
                Value::from(
                    obj.get("delivery_ids")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or_default(),
                ),
            );
        }
        CommunicationRequestAudit::BootstrapConversation => {
            copy_keys(
                obj,
                &mut out,
                &[
                    "agent_id",
                    "endpoint_id",
                    "expected_controller_generation",
                    "conversation_id",
                    "wake_id",
                ],
            );
        }
        CommunicationRequestAudit::ConsumeWake => {
            copy_keys(
                obj,
                &mut out,
                &[
                    "agent_id",
                    "endpoint_id",
                    "expected_controller_generation",
                    "wake_id",
                ],
            );
            let consume_token_present = obj
                .get("consume_token_present")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| obj.get("consume_token").and_then(Value::as_str).is_some());
            out.insert(
                "consume_token_present".to_string(),
                Value::Bool(consume_token_present),
            );
        }
    }
    Value::Object(out)
}

#[derive(Debug, Clone, Copy)]
enum MemoryRequestAudit {
    Search,
    Read,
    Set,
    Delete,
    ScopeList,
    ScopePurge,
}

fn typed_memory_request_audit(kind: MemoryRequestAudit, arguments: &Value) -> Value {
    let Some(obj) = arguments.as_object() else {
        return empty_audit_projection();
    };
    let mut out = serde_json::Map::new();
    if let Some(project) = obj.get("project").cloned() {
        out.insert("project".to_string(), project);
    }
    match kind {
        MemoryRequestAudit::Search => {
            copy_keys(
                obj,
                &mut out,
                &[
                    "project",
                    "offset",
                    "limit",
                    "expected_catalog_revision",
                    "session_id",
                ],
            );
            out.insert(
                "query_present".to_string(),
                Value::Bool(
                    obj.get("query")
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.is_empty()),
                ),
            );
            out.insert(
                "tag_count".to_string(),
                Value::from(
                    obj.get("tags")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or(0),
                ),
            );
        }
        MemoryRequestAudit::Read => {
            copy_keys(
                obj,
                &mut out,
                &["project", "memory_key", "expected_revision", "session_id"],
            );
        }
        MemoryRequestAudit::Set => {
            copy_keys(
                obj,
                &mut out,
                &[
                    "project",
                    "memory_key",
                    "priority",
                    "bootstrap",
                    "expected_revision",
                    "session_id",
                ],
            );
            out.insert(
                "summary_present".to_string(),
                Value::Bool(obj.get("summary").and_then(Value::as_str).is_some()),
            );
            out.insert(
                "body_present".to_string(),
                Value::Bool(obj.get("body").and_then(Value::as_str).is_some()),
            );
            out.insert(
                "tag_count".to_string(),
                Value::from(
                    obj.get("tags")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or(0),
                ),
            );
        }
        MemoryRequestAudit::Delete => {
            copy_keys(
                obj,
                &mut out,
                &["project", "memory_key", "expected_revision", "session_id"],
            );
        }
        MemoryRequestAudit::ScopeList => {
            copy_keys(obj, &mut out, &["offset", "limit"]);
        }
        MemoryRequestAudit::ScopePurge => {
            copy_keys(
                obj,
                &mut out,
                &["memory_scope_id", "expected_catalog_revision"],
            );
        }
    }
    Value::Object(out)
}

#[derive(Debug, Clone, Copy)]
enum SkillRequestAudit {
    Versions,
    Install,
    Activate,
    RemoveRevision,
}

fn typed_skill_request_audit(kind: SkillRequestAudit, arguments: &Value) -> Value {
    let Some(obj) = arguments.as_object() else {
        return empty_audit_projection();
    };
    let mut out = serde_json::Map::new();
    if let Some(project) = obj.get("project").cloned() {
        out.insert("project".to_string(), project);
    }
    match kind {
        SkillRequestAudit::Versions => {
            copy_keys(
                obj,
                &mut out,
                &["project", "skill_key", "offset", "limit", "session_id"],
            );
        }
        SkillRequestAudit::Install => {
            copy_keys(
                obj,
                &mut out,
                &[
                    "project",
                    "skill_key",
                    "expected_artifact_sha256",
                    "activate",
                    "expected_state_revision",
                    "session_id",
                ],
            );
            out.insert(
                "artifact_path_present".to_string(),
                Value::Bool(obj.get("artifact_path").and_then(Value::as_str).is_some()),
            );
            out.insert(
                "idempotency_key_present".to_string(),
                Value::Bool(obj.get("idempotency_key").and_then(Value::as_str).is_some()),
            );
        }
        SkillRequestAudit::Activate => {
            copy_keys(
                obj,
                &mut out,
                &[
                    "project",
                    "skill_key",
                    "package_revision",
                    "expected_state_revision",
                    "session_id",
                ],
            );
            out.insert(
                "idempotency_key_present".to_string(),
                Value::Bool(obj.get("idempotency_key").and_then(Value::as_str).is_some()),
            );
        }
        SkillRequestAudit::RemoveRevision => {
            copy_keys(
                obj,
                &mut out,
                &[
                    "project",
                    "skill_key",
                    "package_revision",
                    "expected_state_revision",
                    "session_id",
                ],
            );
            out.insert(
                "idempotency_key_present".to_string(),
                Value::Bool(obj.get("idempotency_key").and_then(Value::as_str).is_some()),
            );
        }
    }
    Value::Object(out)
}

pub fn session_log_result_for_tool(tool_name: &str, output: &Value) -> Value {
    let Some(definition) = webcodex_tool_contracts::lookup_tool_definition(tool_name) else {
        return empty_audit_projection();
    };
    match definition.audit_policy().result {
        webcodex_tool_contracts::ToolAuditResultPolicy::CanonicalLedgerEvidence => output.clone(),
        webcodex_tool_contracts::ToolAuditResultPolicy::Fields(fields) => {
            project_declared_result_fields(fields, output)
        }
        webcodex_tool_contracts::ToolAuditResultPolicy::Semantic(
            webcodex_tool_contracts::ToolAuditSemanticResultPolicy::CodingAgentObservation,
        ) => coding_agent_observation_result_audit(output),
    }
}

fn project_declared_result_fields(
    fields: &[webcodex_tool_contracts::ToolAuditResultField],
    output: &Value,
) -> Value {
    use webcodex_tool_contracts::ToolAuditResultField;

    let mut projected = serde_json::Map::new();
    for field in fields {
        let (key, value) = match *field {
            ToolAuditResultField::Value {
                output: key,
                source,
            } => (key, output.get(source).cloned().unwrap_or(Value::Null)),
            ToolAuditResultField::Pointer {
                output: key,
                pointer,
            } => (key, output.pointer(pointer).cloned().unwrap_or(Value::Null)),
            ToolAuditResultField::ArrayLen {
                output: key,
                source,
            } => (
                key,
                output
                    .get(source)
                    .and_then(Value::as_array)
                    .map(|items| Value::from(items.len()))
                    .unwrap_or(Value::Null),
            ),
            ToolAuditResultField::PointerArrayLen {
                output: key,
                pointer,
            } => (
                key,
                output
                    .pointer(pointer)
                    .and_then(Value::as_array)
                    .map(|items| Value::from(items.len()))
                    .unwrap_or(Value::Null),
            ),
            ToolAuditResultField::StringBytes {
                output: key,
                source,
            } => (
                key,
                output
                    .get(source)
                    .and_then(Value::as_str)
                    .map(|value| Value::from(value.len()))
                    .unwrap_or(Value::Null),
            ),
            ToolAuditResultField::Presence {
                output: key,
                source,
            } => (key, Value::Bool(output.get(source).is_some())),
            ToolAuditResultField::StringPresent {
                output: key,
                source,
            } => (
                key,
                Value::Bool(output.get(source).and_then(Value::as_str).is_some()),
            ),
            ToolAuditResultField::PointerNonNull {
                output: key,
                pointer,
            } => (
                key,
                Value::Bool(
                    output
                        .pointer(pointer)
                        .is_some_and(|value| !value.is_null()),
                ),
            ),
        };
        projected.insert(key.to_string(), value);
    }
    Value::Object(projected)
}

fn coding_agent_observation_result_audit(output: &Value) -> Value {
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

fn insert_structured_validation_target(
    tool_name: &str,
    arguments: &serde_json::Map<String, Value>,
    out: &mut serde_json::Map<String, Value>,
) {
    if let Some(identity) =
        structured_validation_target_identity(tool_name, &Value::Object(arguments.clone()))
    {
        out.insert("validation_target_id".to_string(), Value::String(identity));
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
    fn request_audit_fails_closed_and_never_mutates_business_arguments() {
        // Structured validation lifecycle controls are audit metadata, not
        // validation identity inputs. Keep the explicit synchronous grace in
        // the bounded request projection while preserving the business args.
        let cargo_test = json!({
            "project": "agent:test:demo",
            "filter": "focused",
            "timeout_secs": 600,
            "sync_wait_secs": 1
        });
        let cargo_test_before = cargo_test.clone();
        let cargo_test_summary = session_log_arguments_for_tool_request("cargo_test", &cargo_test);
        assert_eq!(cargo_test_summary["sync_wait_secs"], 1);
        assert_eq!(cargo_test_summary["timeout_secs"], 600);
        let validation_target_id = cargo_test_summary["validation_target_id"]
            .as_str()
            .expect("cargo_test validation target");
        let mut later_grace = cargo_test.clone();
        later_grace["sync_wait_secs"] = json!(60);
        let later_summary = session_log_arguments_for_tool_request("cargo_test", &later_grace);
        assert_eq!(later_summary["sync_wait_secs"], 60);
        assert_eq!(later_summary["validation_target_id"], validation_target_id);
        assert_eq!(cargo_test, cargo_test_before);

        let unknown = json!({"secret": "UNKNOWN_TOOL_SECRET"});
        let unknown_before = unknown.clone();
        assert_eq!(
            session_log_arguments_for_tool_request("future_unknown_tool", &unknown),
            json!({})
        );
        assert_eq!(unknown, unknown_before);

        let malformed = json!({"secret": "MALFORMED_READ_SECRET"});
        let malformed_before = malformed.clone();
        assert_eq!(
            session_log_arguments_for_tool_request("read_file", &malformed),
            json!({})
        );
        assert_eq!(malformed, malformed_before);

        let retired_alias = json!({"client_id": "PRIVATE_RETIRED_ALIAS_CLIENT"});
        assert_eq!(
            session_log_arguments_for_tool_request("list_agents", &retired_alias),
            json!({}),
            "retired aliases must not become a second audit identity"
        );
    }

    #[test]
    fn request_audit_omits_plugin_ssh_native_path_and_observation_secrets() {
        const PLUGIN_SECRET: &str = "PLUGIN_ARGUMENT_SECRET";
        const SSH_TARGET: &str = "root@private-native-target";
        const SSH_CWD: &str = "/private/native/default/cwd";
        const PROJECT_PATH: &str = "/private/native/project/root";
        const JOB_TOKEN: &str = "wjob-private-observation-token";

        let plugin_binding = format!("wc_pbind_{}", "a".repeat(32));
        let plugin = json!({
            "action": "call",
            "binding": plugin_binding,
            "arguments": {
                "secret": PLUGIN_SECRET,
                "nested": {"token": "PRIVATE_PLUGIN_TOKEN"}
            }
        });
        let plugin_before = plugin.clone();
        let plugin_summary = session_log_arguments_for_tool_request("plugin_tool", &plugin);
        assert_eq!(plugin, plugin_before);
        assert_eq!(plugin_summary["action"], "call");
        assert_eq!(plugin_summary["binding_present"], true);
        assert_eq!(plugin_summary["arguments_present"], true);
        assert_eq!(plugin_summary["argument_key_count"], 2);
        let plugin_serialized = serde_json::to_string(&plugin_summary).unwrap();
        assert!(!plugin_serialized.contains(PLUGIN_SECRET));
        assert!(!plugin_serialized.contains("PRIVATE_PLUGIN_TOKEN"));
        assert!(!plugin_serialized.contains("wc_pbind_"));

        let ssh = json!({
            "action": "register",
            "runner": "private-runner",
            "binding": format!("wc_sbind_{}", "b".repeat(32)),
            "name": "private-ssh-name",
            "target": SSH_TARGET,
            "default_cwd": SSH_CWD
        });
        let ssh_summary = session_log_arguments_for_tool_request("ssh_resource", &ssh);
        assert_eq!(ssh_summary["action"], "register");
        for key in [
            "runner_present",
            "binding_present",
            "name_present",
            "target_present",
            "default_cwd_present",
        ] {
            assert_eq!(ssh_summary[key], true, "missing SSH presence bit {key}");
        }
        let ssh_serialized = serde_json::to_string(&ssh_summary).unwrap();
        for secret in [
            SSH_TARGET,
            SSH_CWD,
            "private-runner",
            "private-ssh-name",
            "wc_sbind_",
        ] {
            assert!(!ssh_serialized.contains(secret));
        }

        let register = json!({
            "client_id": "special",
            "id": "demo",
            "name": "Demo",
            "path": PROJECT_PATH,
            "description": "PRIVATE PROJECT DESCRIPTION",
            "allow_patch": true,
            "overwrite": false
        });
        let register_summary =
            session_log_arguments_for_tool_request("register_project", &register);
        assert_eq!(register_summary["path_present"], true);
        assert_eq!(register_summary["description_present"], true);
        let register_serialized = serde_json::to_string(&register_summary).unwrap();
        assert!(!register_serialized.contains(PROJECT_PATH));
        assert!(!register_serialized.contains("PRIVATE PROJECT DESCRIPTION"));

        let job_log = json!({
            "job_id": "job-safe",
            "after_observation_token": JOB_TOKEN,
            "tail_lines": 20,
            "wait_secs": 1
        });
        let job_summary = session_log_arguments_for_tool_request("job_log", &job_log);
        assert_eq!(job_summary["job_id"], "job-safe");
        assert_eq!(job_summary["token_present"], true);
        assert!(!serde_json::to_string(&job_summary)
            .unwrap()
            .contains(JOB_TOKEN));
    }

    #[test]
    fn result_audit_fails_closed_for_unknown_tools_without_mutating_business_output() {
        let output = json!({
            "secret": "UNKNOWN_RESULT_SECRET",
            "nested": {"token": "PRIVATE_RESULT_TOKEN"}
        });
        let before = output.clone();
        let projected = session_log_result_for_tool("future_unknown_tool", &output);
        assert_eq!(projected, json!({}));
        assert_eq!(output, before);
        assert_eq!(
            session_log_result_for_tool("list_agents", &output),
            json!({}),
            "retired aliases must not inherit a canonical result audit policy"
        );
        let serialized = serde_json::to_string(&projected).unwrap();
        assert!(!serialized.contains("UNKNOWN_RESULT_SECRET"));
        assert!(!serialized.contains("PRIVATE_RESULT_TOKEN"));
    }

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

impl ToolCall {
    pub fn session_log_arguments(&self) -> Value {
        match self {
            Self::RunProcess {
                project,
                executable,
                args,
                stdin,
                timeout_secs,
                sync_wait_secs,
                cwd,
                purpose,
                ..
            } => {
                let identity = run_process_validation_identity(
                    executable,
                    args,
                    stdin.as_deref(),
                    cwd.as_deref(),
                    purpose.as_ref().map(|purpose| purpose.as_str()),
                );
                let mut value = serde_json::json!({
                    "project": project,
                    "executable_present": true,
                    "arg_count": args.len(),
                    "stdin_present": stdin.is_some(),
                    "process_summary": process_preview(
                        executable,
                        args.iter().map(String::as_str),
                    ),
                    "timeout_secs": timeout_secs,
                    "sync_wait_secs": sync_wait_secs,
                    "cwd": cwd,
                    "purpose": purpose,
                });
                if let Some(identity) = identity {
                    value["execution_identity"] = serde_json::json!(identity.identity);
                    if identity.validation_tool.is_some() {
                        value["validation_target_id"] = value["execution_identity"].clone();
                        value["validation_tool"] = serde_json::json!(identity.validation_tool);
                    }
                }
                value
            }
            Self::CodingAgentStart {
                project,
                provider_id,
                idempotency_key,
                instruction,
                config,
                timeout_secs,
                recording_session_id: _,
            } => serde_json::json!({
                "project": project,
                "provider_id": provider_id,
                "idempotency_key_present": !idempotency_key.is_empty(),
                "instruction_bytes": instruction.len(),
                "config_count": config.as_ref().map(std::collections::BTreeMap::len).unwrap_or_default(),
                "timeout_secs": timeout_secs,
            }),
            Self::CodingAgentObserve {
                run_id,
                after_observation_token,
                wait_secs,
            } => serde_json::json!({
                "run_id": run_id,
                "token_present": after_observation_token.is_some(),
                "wait_secs": wait_secs,
            }),
            Self::CodingAgentCancel { run_id } => serde_json::json!({
                "run_id": run_id,
            }),
            Self::RunScript {
                project,
                language,
                script,
                args,
                stdin,
                timeout_secs,
                sync_wait_secs,
                cwd,
                purpose,
                ..
            } => {
                let identity = run_script_validation_identity(
                    language.as_str(),
                    script,
                    args,
                    stdin.as_deref(),
                    cwd.as_deref(),
                    purpose.as_ref().map(|purpose| purpose.as_str()),
                );
                let mut value = serde_json::json!({
                    "project": project,
                    "language": language,
                    "script_bytes": script.len(),
                    "arg_count": args.len(),
                    "stdin_present": stdin.is_some(),
                    "timeout_secs": timeout_secs,
                    "sync_wait_secs": sync_wait_secs,
                    "cwd": cwd,
                    "purpose": purpose,
                });
                if let Some(identity) = identity {
                    value["execution_identity"] = serde_json::json!(identity.identity);
                    if identity.validation_tool.is_some() {
                        value["validation_target_id"] = value["execution_identity"].clone();
                        value["validation_tool"] = serde_json::json!(identity.validation_tool);
                    }
                }
                value
            }
            Self::RunShell {
                project,
                command,
                timeout_secs,
                cwd,
                purpose,
                shell,
                ..
            } => serde_json::json!({
                "project": project,
                "command_present": true,
                "command_summary": command_preview(command),
                "timeout_secs": timeout_secs,
                "cwd": cwd,
                "purpose": purpose,
                "shell": shell,
            }),
            Self::RunJob {
                project,
                command,
                timeout_secs,
                cwd,
                purpose,
                shell,
                ..
            } => serde_json::json!({
                "project": project,
                "command_present": true,
                "command_summary": command_preview(command),
                "timeout_secs": timeout_secs,
                "cwd": cwd,
                "purpose": purpose,
                "shell": shell,
            }),
            Self::OpenSessionShell {
                project,
                session_id,
                cwd,
                shell,
            } => serde_json::json!({
                "project": project,
                "session_id": session_id,
                "cwd": cwd,
                "shell": shell,
            }),
            Self::SessionShellExec {
                project,
                session_id,
                shell_id,
                command,
                timeout_secs,
                purpose,
            } => serde_json::json!({
                "project": project,
                "session_id": session_id,
                "shell_id": shell_id,
                "command_present": true,
                "command_summary": command_preview(command),
                "timeout_secs": timeout_secs,
                "purpose": purpose,
            }),
            Self::SessionShellStatus {
                project,
                session_id,
                shell_id,
            }
            | Self::CloseSessionShell {
                project,
                session_id,
                shell_id,
            } => serde_json::json!({
                "project": project,
                "session_id": session_id,
                "shell_id": shell_id,
            }),
            Self::ComputerListTargets => serde_json::json!({}),
            Self::ComputerListWindows { client_id, limit } => serde_json::json!({
                "client_id": client_id,
                "limit": limit,
            }),
            Self::ComputerListApplications { client_id, limit } => serde_json::json!({
                "client_id": client_id,
                "limit": limit,
            }),
            Self::ComputerLaunchApplication {
                client_id,
                application_id,
            } => serde_json::json!({
                "client_id": client_id,
                "application_id": application_id,
            }),
            Self::ComputerAccessibilityStatus { client_id } => serde_json::json!({
                "client_id": client_id,
            }),
            Self::ComputerAccessibilityTree {
                client_id,
                surface_id,
                max_depth,
                max_nodes,
            } => serde_json::json!({
                "client_id": client_id,
                "surface_id": surface_id,
                "max_depth": max_depth,
                "max_nodes": max_nodes,
            }),
            Self::ComputerFindElements {
                client_id,
                surface_id,
                role,
                subrole,
                label,
                focused,
                enabled,
                limit,
            } => serde_json::json!({
                "client_id": client_id,
                "surface_id": surface_id,
                "role_present": role.is_some(),
                "subrole_present": subrole.is_some(),
                "label_present": label.is_some(),
                "focused": focused,
                "enabled": enabled,
                "limit": limit,
            }),
            Self::ComputerElementState {
                client_id,
                surface_id,
                element_id,
            } => serde_json::json!({
                "client_id": client_id,
                "surface_id": surface_id,
                "element_id": element_id,
            }),
            Self::ComputerActivateWindow {
                client_id,
                surface_id,
            } => serde_json::json!({
                "client_id": client_id,
                "surface_id": surface_id,
            }),
            Self::ComputerControl {
                client_id,
                surface_id,
                element_id,
                action,
            } => serde_json::json!({
                "client_id": client_id,
                "surface_id": surface_id,
                "element_id": element_id,
                "action": action,
            }),
            Self::ComputerScrollToElement {
                client_id,
                surface_id,
                element_id,
            } => serde_json::json!({
                "client_id": client_id,
                "surface_id": surface_id,
                "element_id": element_id,
            }),
            Self::ComputerKeyInput {
                client_id,
                surface_id,
                key,
                modifiers,
            } => serde_json::json!({
                "client_id": client_id,
                "surface_id": surface_id,
                "key": key,
                "modifiers": modifiers,
            }),
            Self::ComputerInputText {
                client_id,
                surface_id,
                element_id,
                text,
            } => serde_json::json!({
                "client_id": client_id,
                "surface_id": surface_id,
                "element_id": element_id,
                "text_bytes": text.len(),
            }),
            Self::ComputerSnapshot {
                client_id,
                surface_id,
                region,
                max_width,
                max_height,
            } => serde_json::json!({
                "client_id": client_id,
                "surface_id": surface_id,
                "region_present": region.is_some(),
                "max_width": max_width,
                "max_height": max_height,
            }),
            Self::ComputerSaveSnapshot {
                project,
                path,
                client_id,
                surface_id,
                region,
                max_width,
                max_height,
                ..
            } => serde_json::json!({
                "project": project,
                "path": path,
                "client_id": client_id,
                "surface_id": surface_id,
                "region_present": region.is_some(),
                "max_width": max_width,
                "max_height": max_height,
            }),
            Self::StopJob {
                project,
                job_id,
                confirm,
                ..
            } => serde_json::json!({
                "project": project,
                "job_id": job_id,
                "confirm": confirm,
            }),
            Self::ObserveJobs {
                items,
                tail_lines,
                wait_secs,
            } => serde_json::json!({
                "item_count": items.len(),
                "token_count": items
                    .iter()
                    .filter(|item| item.after_observation_token.is_some())
                    .count(),
                "job_ids": items
                    .iter()
                    .map(|item| item.job_id.as_str())
                    .collect::<Vec<_>>(),
                "tail_lines": tail_lines,
                "wait_secs": wait_secs,
            }),
            Self::ApplyUnifiedDiff {
                project,
                deny_sensitive_paths,
                ..
            } => serde_json::json!({
                "project": project,
                "diff_present": true,
                "deny_sensitive_paths": deny_sensitive_paths,
            }),
            Self::DeleteProjectFiles { project, paths, .. }
            | Self::GitRestorePaths { project, paths, .. }
            | Self::DiscardUntracked { project, paths, .. } => serde_json::json!({
                "project": project,
                "paths": paths,
            }),
            Self::GitCommitPaths {
                project,
                expected_head,
                paths,
                ..
            } => {
                let expected_head = normalized_exact_git_commit_for_audit(expected_head);
                serde_json::json!({
                    "project": project,
                    "paths": paths,
                    "expected_head_valid": expected_head.is_some(),
                    "expected_head": expected_head,
                    "message_present": true,
                })
            }
            Self::GitStatus { project, .. } | Self::GitDiffSummary { project, .. } => {
                serde_json::json!({
                    "project": project,
                })
            }
            Self::GitReviewSummary {
                project,
                base_commit,
                head_commit,
                ..
            } => {
                let base_commit = normalized_exact_git_commit_for_audit(base_commit);
                let head_commit = normalized_exact_git_commit_for_audit(head_commit);
                serde_json::json!({
                    "project": project,
                    "base_commit_valid": base_commit.is_some(),
                    "base_commit": base_commit,
                    "head_commit_valid": head_commit.is_some(),
                    "head_commit": head_commit,
                })
            }
            Self::GitLog {
                project,
                limit,
                skip,
                ..
            } => serde_json::json!({
                "project": project,
                "limit": limit,
                "skip": skip,
            }),
            Self::GitDiff { project, args, .. } => serde_json::json!({
                "project": project,
                "args_count": args.as_ref().map(Vec::len),
            }),
            Self::GitDiffHunks {
                project,
                paths,
                max_hunks,
                max_hunk_lines,
                cached,
                base_commit,
                head_commit,
                continuation,
                ..
            } => {
                let base_commit = base_commit
                    .as_deref()
                    .and_then(normalized_exact_git_commit_for_audit);
                let head_commit = head_commit
                    .as_deref()
                    .and_then(normalized_exact_git_commit_for_audit);
                let mut out = serde_json::json!({
                    "project": project,
                    "paths": paths,
                    "max_hunks": max_hunks,
                    "max_hunk_lines": max_hunk_lines,
                    "cached": cached,
                    "base_commit_valid": base_commit.is_some(),
                    "head_commit_valid": head_commit.is_some(),
                    "continuation_present": continuation.is_some(),
                });
                if let Some(base_commit) = base_commit {
                    out["base_commit"] = Value::String(base_commit);
                }
                if let Some(head_commit) = head_commit {
                    out["head_commit"] = Value::String(head_commit);
                }
                out
            }
            Self::CargoFmt {
                project,
                cwd,
                check,
                timeout_secs,
                sync_wait_secs,
                ..
            } => typed_structured_validation_request_audit(
                StructuredValidationRequestAudit::CargoFmt,
                &serde_json::json!({
                    "project": project,
                    "cwd": cwd,
                    "check": check,
                    "timeout_secs": timeout_secs,
                    "sync_wait_secs": sync_wait_secs,
                }),
            ),
            Self::CargoCheck {
                project,
                cwd,
                all_targets,
                all_features,
                no_default_features,
                features,
                package,
                timeout_secs,
                sync_wait_secs,
                ..
            } => typed_structured_validation_request_audit(
                StructuredValidationRequestAudit::CargoCheck,
                &serde_json::json!({
                    "project": project,
                    "cwd": cwd,
                    "all_targets": all_targets,
                    "all_features": all_features,
                    "no_default_features": no_default_features,
                    "features": features,
                    "package": package,
                    "timeout_secs": timeout_secs,
                    "sync_wait_secs": sync_wait_secs,
                }),
            ),
            Self::CargoTest {
                project,
                cwd,
                filter,
                all_targets,
                all_features,
                no_default_features,
                features,
                package,
                no_run,
                require_tests,
                min_tests,
                timeout_secs,
                sync_wait_secs,
                ..
            } => typed_structured_validation_request_audit(
                StructuredValidationRequestAudit::CargoTest,
                &serde_json::json!({
                    "project": project,
                    "cwd": cwd,
                    "filter": filter,
                    "all_targets": all_targets,
                    "all_features": all_features,
                    "no_default_features": no_default_features,
                    "features": features,
                    "package": package,
                    "no_run": no_run,
                    "require_tests": require_tests,
                    "min_tests": min_tests,
                    "timeout_secs": timeout_secs,
                    "sync_wait_secs": sync_wait_secs,
                }),
            ),
            Self::GoTest {
                project,
                cwd,
                packages,
                timeout_secs,
                sync_wait_secs,
                ..
            } => typed_structured_validation_request_audit(
                StructuredValidationRequestAudit::GoTest,
                &serde_json::json!({
                    "project": project,
                    "cwd": cwd,
                    "packages": packages,
                    "timeout_secs": timeout_secs,
                    "sync_wait_secs": sync_wait_secs,
                }),
            ),
            Self::ReadFile {
                project,
                path,
                start_line,
                limit,
                with_line_numbers,
                ..
            } => serde_json::json!({
                "project": project,
                "path": path,
                "start_line": start_line,
                "limit": limit,
                "with_line_numbers": with_line_numbers,
            }),
            Self::ReadFiles {
                project,
                items,
                with_line_numbers,
                ..
            } => serde_json::json!({
                "project": project,
                "items": items,
                "with_line_numbers": with_line_numbers,
            }),
            Self::CreateAgentTask {
                title,
                instruction,
                assignee_agent_id,
                source_conversation_id,
                source_message_id,
                referenced_project_id,
                idempotency_key,
            } => typed_agent_task_request_audit(
                AgentTaskRequestAudit::Create,
                &serde_json::json!({
                    "title": title,
                    "instruction": instruction,
                    "assignee_agent_id": assignee_agent_id,
                    "source_conversation_id": source_conversation_id,
                    "source_message_id": source_message_id,
                    "referenced_project_id": referenced_project_id,
                    "idempotency_key": idempotency_key,
                }),
            ),
            Self::ListAgentTasks {
                assignee_agent_id,
                offset,
                limit,
            } => typed_agent_task_request_audit(
                AgentTaskRequestAudit::List,
                &serde_json::json!({
                    "assignee_agent_id": assignee_agent_id,
                    "offset": offset,
                    "limit": limit,
                }),
            ),
            Self::ReadAgentTask { task_id } => typed_agent_task_request_audit(
                AgentTaskRequestAudit::Read,
                &serde_json::json!({"task_id": task_id}),
            ),
            Self::AssignAgentTask {
                task_id,
                assignee_agent_id,
            } => typed_agent_task_request_audit(
                AgentTaskRequestAudit::Assign,
                &serde_json::json!({
                    "task_id": task_id,
                    "assignee_agent_id": assignee_agent_id,
                }),
            ),
            Self::StartAgentTaskAttempt {
                task_id,
                assignee_agent_id,
                idempotency_key,
            } => typed_agent_task_request_audit(
                AgentTaskRequestAudit::StartAttempt,
                &serde_json::json!({
                    "task_id": task_id,
                    "assignee_agent_id": assignee_agent_id,
                    "idempotency_key": idempotency_key,
                }),
            ),
            Self::StartAgentTaskCodingRun {
                project,
                task_id,
                attempt_id,
                assignee_agent_id,
                attempt_fence,
                attempt_controller_generation,
                provider_id,
                config,
                timeout_secs,
            } => typed_agent_task_request_audit(
                AgentTaskRequestAudit::StartCodingRun,
                &serde_json::json!({
                    "project": project,
                    "task_id": task_id,
                    "attempt_id": attempt_id,
                    "assignee_agent_id": assignee_agent_id,
                    "attempt_fence": attempt_fence,
                    "attempt_controller_generation": attempt_controller_generation,
                    "provider_id": provider_id,
                    "config": config,
                    "timeout_secs": timeout_secs,
                }),
            ),
            Self::ReconcileAgentTaskCodingRun {
                task_id,
                attempt_id,
            } => typed_agent_task_request_audit(
                AgentTaskRequestAudit::ReconcileCodingRun,
                &serde_json::json!({
                    "task_id": task_id,
                    "attempt_id": attempt_id,
                }),
            ),
            Self::HeartbeatAgentTaskAttempt {
                task_id,
                attempt_id,
                assignee_agent_id,
                attempt_fence,
                attempt_controller_generation,
            } => typed_agent_task_request_audit(
                AgentTaskRequestAudit::HeartbeatAttempt,
                &serde_json::json!({
                    "task_id": task_id,
                    "attempt_id": attempt_id,
                    "assignee_agent_id": assignee_agent_id,
                    "attempt_fence": attempt_fence,
                    "attempt_controller_generation": attempt_controller_generation,
                }),
            ),
            Self::CompleteAgentTaskAttempt {
                task_id,
                attempt_id,
                assignee_agent_id,
                attempt_fence,
                attempt_controller_generation,
                outcome,
                terminal_result,
                terminal_reason,
                completion_key,
            } => typed_agent_task_request_audit(
                AgentTaskRequestAudit::CompleteAttempt,
                &serde_json::json!({
                    "task_id": task_id,
                    "attempt_id": attempt_id,
                    "assignee_agent_id": assignee_agent_id,
                    "attempt_fence": attempt_fence,
                    "attempt_controller_generation": attempt_controller_generation,
                    "outcome": outcome,
                    "terminal_result": terminal_result,
                    "terminal_reason": terminal_reason,
                    "completion_key": completion_key,
                }),
            ),
            Self::CreateAgentIdentity {
                handle,
                display_name,
                description,
                specialty_labels,
                idempotency_key,
            } => typed_communication_request_audit(
                CommunicationRequestAudit::CreateIdentity,
                &serde_json::json!({
                    "handle": handle,
                    "display_name": display_name,
                    "description": description,
                    "specialty_labels": specialty_labels,
                    "idempotency_key": idempotency_key,
                }),
            ),
            Self::ListAgentIdentities {
                agent_id,
                offset,
                limit,
            } => typed_communication_request_audit(
                CommunicationRequestAudit::ListIdentities,
                &serde_json::json!({"agent_id": agent_id, "offset": offset, "limit": limit}),
            ),
            Self::UpdateAgentIdentity {
                agent_id,
                expected_profile_revision,
                handle,
                display_name,
                description,
                specialty_labels,
            } => typed_communication_request_audit(
                CommunicationRequestAudit::UpdateIdentity,
                &serde_json::json!({
                    "agent_id": agent_id,
                    "expected_profile_revision": expected_profile_revision,
                    "handle": handle,
                    "display_name": display_name,
                    "description": description,
                    "specialty_labels": specialty_labels,
                }),
            ),
            Self::AttachAgentEndpoint {
                agent_id,
                host,
                client_attachment_id,
                idempotency_key,
            } => typed_communication_request_audit(
                CommunicationRequestAudit::AttachEndpoint,
                &serde_json::json!({
                    "agent_id": agent_id,
                    "host": host,
                    "client_attachment_id": client_attachment_id,
                    "idempotency_key": idempotency_key,
                }),
            ),
            Self::DetachAgentEndpoint { endpoint_id } => typed_communication_request_audit(
                CommunicationRequestAudit::DetachEndpoint,
                &serde_json::json!({"endpoint_id": endpoint_id}),
            ),
            Self::CreateConversation {
                title,
                agent_ids,
                idempotency_key,
            } => typed_communication_request_audit(
                CommunicationRequestAudit::CreateConversation,
                &serde_json::json!({
                    "title": title,
                    "agent_ids": agent_ids,
                    "idempotency_key": idempotency_key,
                }),
            ),
            Self::ListConversations {
                agent_id,
                endpoint_id,
                expected_controller_generation,
                offset,
                limit,
            } => typed_communication_request_audit(
                CommunicationRequestAudit::ListConversations,
                &serde_json::json!({
                    "agent_id": agent_id,
                    "endpoint_id": endpoint_id,
                    "expected_controller_generation": expected_controller_generation,
                    "offset": offset,
                    "limit": limit,
                }),
            ),
            Self::ReadConversation {
                conversation_id,
                agent_id,
                endpoint_id,
                expected_controller_generation,
                after_seq,
                limit,
            } => typed_communication_request_audit(
                CommunicationRequestAudit::ReadConversation,
                &serde_json::json!({
                    "conversation_id": conversation_id,
                    "agent_id": agent_id,
                    "endpoint_id": endpoint_id,
                    "expected_controller_generation": expected_controller_generation,
                    "after_seq": after_seq,
                    "limit": limit,
                }),
            ),
            Self::PostConversationMessage {
                conversation_id,
                body,
                author_agent_id,
                endpoint_id,
                expected_controller_generation,
                recipient_agent_ids,
                reply_to,
                idempotency_key,
                wake_reply_id,
                reply_operation_index,
            } => typed_communication_request_audit(
                CommunicationRequestAudit::PostMessage,
                &serde_json::json!({
                    "conversation_id": conversation_id,
                    "body": body,
                    "author_agent_id": author_agent_id,
                    "endpoint_id": endpoint_id,
                    "expected_controller_generation": expected_controller_generation,
                    "recipient_agent_ids": recipient_agent_ids,
                    "reply_to": reply_to,
                    "idempotency_key": idempotency_key,
                    "wake_reply_id": wake_reply_id,
                    "reply_operation_index": reply_operation_index,
                }),
            ),
            Self::ListAgentInbox {
                agent_id,
                endpoint_id,
                expected_controller_generation,
                after_delivery_order,
                limit,
            } => typed_communication_request_audit(
                CommunicationRequestAudit::ListInbox,
                &serde_json::json!({
                    "agent_id": agent_id,
                    "endpoint_id": endpoint_id,
                    "expected_controller_generation": expected_controller_generation,
                    "after_delivery_order": after_delivery_order,
                    "limit": limit,
                }),
            ),
            Self::ConsumeAgentDeliveries {
                agent_id,
                endpoint_id,
                expected_controller_generation,
                delivery_ids,
            } => typed_communication_request_audit(
                CommunicationRequestAudit::ConsumeDeliveries,
                &serde_json::json!({
                    "agent_id": agent_id,
                    "endpoint_id": endpoint_id,
                    "expected_controller_generation": expected_controller_generation,
                    "delivery_ids": delivery_ids,
                }),
            ),
            Self::BootstrapAgentConversation {
                agent_id,
                endpoint_id,
                expected_controller_generation,
                conversation_id,
                wake_id,
                activation_idempotency_key,
            } => typed_communication_request_audit(
                CommunicationRequestAudit::BootstrapConversation,
                &serde_json::json!({
                    "agent_id": agent_id,
                    "endpoint_id": endpoint_id,
                    "expected_controller_generation": expected_controller_generation,
                    "conversation_id": conversation_id,
                    "wake_id": wake_id,
                    "activation_idempotency_key": activation_idempotency_key,
                }),
            ),
            Self::ConsumeAgentWake {
                agent_id,
                endpoint_id,
                expected_controller_generation,
                wake_id,
                consume_token,
            } => typed_communication_request_audit(
                CommunicationRequestAudit::ConsumeWake,
                &serde_json::json!({
                    "agent_id": agent_id,
                    "endpoint_id": endpoint_id,
                    "expected_controller_generation": expected_controller_generation,
                    "wake_id": wake_id,
                    "consume_token_present": !consume_token.is_empty(),
                }),
            ),
            Self::MemorySearch {
                project,
                query,
                tags,
                offset,
                limit,
                expected_catalog_revision,
                session_id,
            } => typed_memory_request_audit(
                MemoryRequestAudit::Search,
                &serde_json::json!({
                    "project": project,
                    "query": query,
                    "tags": tags,
                    "offset": offset,
                    "limit": limit,
                    "expected_catalog_revision": expected_catalog_revision,
                    "session_id": session_id,
                }),
            ),
            Self::MemoryRead {
                project,
                memory_key,
                expected_revision,
                session_id,
            } => typed_memory_request_audit(
                MemoryRequestAudit::Read,
                &serde_json::json!({
                    "project": project,
                    "memory_key": memory_key,
                    "expected_revision": expected_revision,
                    "session_id": session_id,
                }),
            ),
            Self::MemorySet {
                project,
                memory_key,
                summary,
                body,
                priority,
                bootstrap,
                tags,
                expected_revision,
                session_id,
            } => typed_memory_request_audit(
                MemoryRequestAudit::Set,
                &serde_json::json!({
                    "project": project,
                    "memory_key": memory_key,
                    "summary": summary,
                    "body": body,
                    "priority": priority,
                    "bootstrap": bootstrap,
                    "tags": tags,
                    "expected_revision": expected_revision,
                    "session_id": session_id,
                }),
            ),
            Self::MemoryDelete {
                project,
                memory_key,
                expected_revision,
                session_id,
            } => typed_memory_request_audit(
                MemoryRequestAudit::Delete,
                &serde_json::json!({
                    "project": project,
                    "memory_key": memory_key,
                    "expected_revision": expected_revision,
                    "session_id": session_id,
                }),
            ),
            Self::MemoryScopeList { offset, limit } => typed_memory_request_audit(
                MemoryRequestAudit::ScopeList,
                &serde_json::json!({"offset": offset, "limit": limit}),
            ),
            Self::MemoryScopePurge {
                memory_scope_id,
                expected_catalog_revision,
                ..
            } => typed_memory_request_audit(
                MemoryRequestAudit::ScopePurge,
                &serde_json::json!({
                    "memory_scope_id": memory_scope_id,
                    "expected_catalog_revision": expected_catalog_revision,
                }),
            ),
            Self::SkillList {
                project,
                query,
                offset,
                limit,
                expected_catalog_revision,
                ..
            } => serde_json::json!({
                "project": project,
                "query_present": query.as_ref().is_some_and(|value| !value.is_empty()),
                "offset": offset,
                "limit": limit,
                "expected_catalog_revision": expected_catalog_revision,
            }),
            Self::SkillReadFile {
                project,
                skill_id,
                path,
                start_line,
                limit,
                expected_definition_revision,
                expected_package_revision,
                ..
            } => serde_json::json!({
                "project": project,
                "skill_id": skill_id,
                "path": path,
                "start_line": start_line,
                "limit": limit,
                "expected_definition_revision": expected_definition_revision,
                "expected_package_revision": expected_package_revision,
            }),
            Self::SkillVersions {
                project,
                skill_key,
                offset,
                limit,
                session_id,
            } => typed_skill_request_audit(
                SkillRequestAudit::Versions,
                &serde_json::json!({
                    "project": project,
                    "skill_key": skill_key,
                    "offset": offset,
                    "limit": limit,
                    "session_id": session_id,
                }),
            ),
            Self::SkillInstall {
                project,
                skill_key,
                artifact_path,
                expected_artifact_sha256,
                idempotency_key,
                activate,
                expected_state_revision,
                session_id,
            } => typed_skill_request_audit(
                SkillRequestAudit::Install,
                &serde_json::json!({
                    "project": project,
                    "skill_key": skill_key,
                    "artifact_path": artifact_path,
                    "expected_artifact_sha256": expected_artifact_sha256,
                    "idempotency_key": idempotency_key,
                    "activate": activate,
                    "expected_state_revision": expected_state_revision,
                    "session_id": session_id,
                }),
            ),
            Self::SkillActivate {
                project,
                skill_key,
                package_revision,
                expected_state_revision,
                idempotency_key,
                session_id,
            } => typed_skill_request_audit(
                SkillRequestAudit::Activate,
                &serde_json::json!({
                    "project": project,
                    "skill_key": skill_key,
                    "package_revision": package_revision,
                    "expected_state_revision": expected_state_revision,
                    "idempotency_key": idempotency_key,
                    "session_id": session_id,
                }),
            ),
            Self::SkillRemoveRevision {
                project,
                skill_key,
                package_revision,
                expected_state_revision,
                idempotency_key,
                session_id,
            } => typed_skill_request_audit(
                SkillRequestAudit::RemoveRevision,
                &serde_json::json!({
                    "project": project,
                    "skill_key": skill_key,
                    "package_revision": package_revision,
                    "expected_state_revision": expected_state_revision,
                    "idempotency_key": idempotency_key,
                    "session_id": session_id,
                }),
            ),
            Self::ListProjectFiles {
                project,
                path,
                limit,
                ..
            } => serde_json::json!({
                "project": project,
                "path": path,
                "limit": limit,
            }),
            Self::ProjectOverview {
                project,
                path,
                max_depth,
                limit,
                ..
            } => serde_json::json!({
                "project": project,
                "path": path,
                "max_depth": max_depth,
                "limit": limit,
            }),
            Self::SearchProjectText {
                project,
                path,
                limit,
                context_before,
                context_after,
                include_globs,
                exclude_globs,
                result_mode,
                timeout_secs,
                ..
            } => serde_json::json!({
                "project": project,
                "pattern_present": true,
                "path": path,
                "limit": limit,
                "context_before": context_before,
                "context_after": context_after,
                "include_glob_count": include_globs.as_ref().map(Vec::len).unwrap_or(0),
                "exclude_glob_count": exclude_globs.as_ref().map(Vec::len).unwrap_or(0),
                "result_mode": result_mode,
                "timeout_secs": timeout_secs,
            }),
            Self::SearchProjectTexts {
                project, queries, ..
            } => serde_json::json!({
                "project": project,
                "query_count": queries.len(),
                "patterns_present": !queries.is_empty(),
            }),
            Self::LspStatus { project, .. } => serde_json::json!({
                "project": project,
            }),
            Self::DocumentSymbols {
                project,
                path,
                limit,
                ..
            }
            | Self::DocumentDiagnostics {
                project,
                path,
                limit,
                ..
            } => serde_json::json!({
                "project": project,
                "path": path,
                "limit": limit,
            }),
            Self::Hover {
                project,
                path,
                line,
                column,
                ..
            } => serde_json::json!({
                "project": project,
                "path": path,
                "line": line,
                "column": column,
            }),
            Self::WorkspaceSymbols { project, limit, .. } => serde_json::json!({
                "project": project,
                "query_present": true,
                "limit": limit,
            }),
            Self::GotoDefinition {
                project,
                path,
                line,
                column,
                limit,
                ..
            } => serde_json::json!({
                "project": project,
                "path": path,
                "line": line,
                "column": column,
                "limit": limit,
            }),
            Self::FindReferences {
                project,
                path,
                line,
                column,
                include_declaration,
                limit,
                ..
            } => serde_json::json!({
                "project": project,
                "path": path,
                "line": line,
                "column": column,
                "include_declaration": include_declaration,
                "limit": limit,
            }),
            Self::CallHierarchy {
                project,
                path,
                line,
                column,
                direction,
                depth,
                limit,
                ..
            } => serde_json::json!({
                "project": project,
                "path": path,
                "line": line,
                "column": column,
                "direction": direction,
                "depth": depth,
                "limit": limit,
            }),
            Self::ShowChanges {
                project,
                include_diff,
                max_hunks,
                max_hunk_lines,
                session_event_limit,
                ..
            } => serde_json::json!({
                "project": project,
                "include_diff": include_diff,
                "max_hunks": max_hunks,
                "max_hunk_lines": max_hunk_lines,
                "session_event_limit": session_event_limit,
            }),
            Self::WriteProjectFile {
                project,
                path,
                overwrite,
                expected_sha256,
                ..
            } => serde_json::json!({
                "project": project,
                "path": path,
                "content_present": true,
                "overwrite": overwrite,
                "expected_sha256_present": expected_sha256.as_ref().is_some_and(|v| !v.is_empty()),
            }),
            Self::SaveProjectArtifact {
                project,
                path,
                mime_type,
                overwrite,
                ..
            } => serde_json::json!({
                "project": project,
                "path": path,
                "content_base64_present": true,
                "mime_type": mime_type,
                "overwrite": overwrite,
            }),
            Self::ImportConversationFilesToProject {
                project,
                openai_file_id_refs,
                output_dir,
                targets,
                overwrite,
                session_id,
                ..
            } => serde_json::json!({
                "project": project,
                "file_count": openai_file_id_refs.len(),
                "output_dir": output_dir,
                "targets_count": targets.as_ref().map(Vec::len).unwrap_or_default(),
                "overwrite": overwrite,
                "session_id": session_id,
            }),
            Self::ReadProjectArtifactMetadata {
                project,
                path,
                allow_missing,
                ..
            } => serde_json::json!({
                "project": project,
                "path": path,
                "allow_missing": allow_missing,
            }),
            Self::ReadProjectArtifact {
                project,
                path,
                encoding,
                offset,
                length,
                as_image,
                ..
            } => serde_json::json!({
                "project": project,
                "path": path,
                "encoding": encoding,
                "offset": offset,
                "length": length,
                "as_image": as_image,
            }),
            Self::ArtifactUploadBegin {
                project,
                path,
                expected_bytes,
                expected_sha256,
                mime_type,
                overwrite,
                ..
            } => serde_json::json!({
                "project": project,
                "path": path,
                "expected_bytes": expected_bytes,
                "expected_sha256_present": expected_sha256.as_ref().is_some_and(|v| !v.is_empty()),
                "mime_type": mime_type,
                "overwrite": overwrite,
            }),
            Self::ArtifactUploadChunk {
                project,
                path,
                upload_id,
                offset,
                ..
            } => serde_json::json!({
                "project": project,
                "path": path,
                "upload_id": upload_id,
                "offset": offset,
                "content_base64_present": true,
            }),
            Self::ArtifactUploadFinish {
                project,
                path,
                upload_id,
                ..
            }
            | Self::ArtifactUploadAbort {
                project,
                path,
                upload_id,
                ..
            } => serde_json::json!({
                "project": project,
                "path": path,
                "upload_id": upload_id,
            }),
            Self::ApplyPatch {
                project,
                patch,
                dry_run,
                matching_mode,
                ..
            } => serde_json::json!({
                "project": project,
                "patch_present": !patch.is_empty(),
                "patch_bytes": patch.len(),
                "dry_run": dry_run,
                "matching_mode": matching_mode.map(|mode| mode.as_str()),
            }),
            Self::ApplyTextEdits {
                project,
                changes,
                dry_run,
                ..
            } => {
                let kind_list: Vec<&str> =
                    changes.iter().map(|change| change.kind.as_str()).collect();
                serde_json::json!({
                    "project": project,
                    "change_count": changes.len(),
                    "kinds": kind_list,
                    "paths": changes.iter().map(|change| change.path.as_str()).collect::<Vec<_>>(),
                    "destination_paths": changes.iter().filter_map(|change| change.to_path.as_deref()).collect::<Vec<_>>(),
                    "expected_sha256_count": changes.iter().filter(|change| change.expected_sha256.is_some()).count(),
                    "dry_run": dry_run,
                })
            }
            Self::WorkspaceCheckpointCreate {
                project,
                title,
                note,
                include_untracked,
                kind,
                labels,
                validation,
                ..
            } => {
                let kind = kind
                    .as_deref()
                    .filter(|value| is_checkpoint_kind(value))
                    .unwrap_or(if kind.is_some() {
                        "invalid"
                    } else {
                        "snapshot"
                    });
                let validation_status = validation
                    .as_ref()
                    .and_then(|value| value.status.as_deref())
                    .filter(|value| is_checkpoint_validation_status(value))
                    .unwrap_or(
                        if validation
                            .as_ref()
                            .and_then(|value| value.status.as_deref())
                            .is_some()
                        {
                            "invalid"
                        } else {
                            "unknown"
                        },
                    );
                serde_json::json!({
                    "project": project,
                    "title": title,
                    "note_present": note.as_ref().is_some_and(|v| !v.is_empty()),
                    "include_untracked": include_untracked,
                    "kind": kind,
                    "label_count": labels.len(),
                    "validation_status": validation_status,
                })
            }
            Self::WorkspaceCheckpointList { project, limit, .. } => serde_json::json!({
                "project": project,
                "limit": limit,
            }),
            Self::WorkspaceCheckpointShow {
                project,
                checkpoint_id,
                include_diff_stat,
                ..
            } => serde_json::json!({
                "project": project,
                "checkpoint_id": checkpoint_id,
                "include_diff_stat": include_diff_stat,
            }),
            Self::WorkspaceCheckpointRestore {
                project,
                checkpoint_id,
                confirm,
                ..
            } => serde_json::json!({
                "project": project,
                "checkpoint_id": checkpoint_id,
                "confirm": confirm,
            }),
            Self::WorkspaceCheckpointDelete {
                project,
                checkpoint_id,
                confirm,
                ..
            } => serde_json::json!({
                "project": project,
                "checkpoint_id": checkpoint_id,
                "confirm": confirm,
            }),
            Self::PostSessionMessage {
                session_id,
                kind,
                message,
                tags,
                reply_to,
                priority,
                requires_ack,
            } => serde_json::json!({
                "session_id": session_id,
                "kind": kind,
                "body_present": !message.is_empty(),
                "body_bytes": message.len(),
                "tags_count": tags.len(),
                "reply_to": reply_to,
                "priority": priority,
                "requires_ack": requires_ack,
            }),
            Self::ListSessionMessages {
                session_id,
                kind,
                status,
                message_id,
                reply_to,
                limit,
            } => serde_json::json!({
                "session_id": session_id,
                "kind": kind,
                "status": status,
                "message_id": message_id,
                "reply_to": reply_to,
                "limit": limit,
            }),
            Self::ObserveSessionMessages {
                session_id,
                after_observation_token,
                wait_secs,
                limit,
            } => serde_json::json!({
                "session_id": session_id,
                "token_present": after_observation_token.is_some(),
                "wait_secs": wait_secs,
                "limit": limit,
            }),
            Self::ResolveSessionMessage {
                session_id,
                message_id,
                resolution,
            } => serde_json::json!({
                "session_id": session_id,
                "message_id": message_id,
                "resolution_present": resolution.as_ref().is_some_and(|v| !v.is_empty()),
            }),
            Self::CompleteSessionMessage {
                session_id,
                message_id,
                answer,
                completion_key,
                expected_assignment_fence: _,
                tags,
                priority,
                ..
            } => serde_json::json!({
                "session_id": session_id,
                "message_id": message_id,
                "body_present": true,
                "body_bytes": answer.len(),
                "tags_count": tags.len(),
                "priority": priority,
                "completion_id": bounded_completion_key_fingerprint(Some(completion_key)),
                "assignment_fence_present": true,
            }),
            Self::SessionDiscussionSummary { session_id, limit } => serde_json::json!({
                "session_id": session_id,
                "limit": limit,
            }),
            Self::SessionHandoffSummary {
                session_id,
                project,
                include_workspace,
                include_checkpoints,
                include_validation,
                summary_only,
                limit,
            } => serde_json::json!({
                "session_id": session_id,
                "project": project,
                "include_workspace": include_workspace,
                "include_checkpoints": include_checkpoints,
                "include_validation": include_validation,
                "summary_only": summary_only,
                "limit": limit,
            }),
            Self::StartSession {
                project,
                title,
                mode,
                deny_write_tools,
                deny_shell_tools,
                execution_context,
            } => serde_json::json!({
                "project": project,
                "title": title,
                "mode": mode,
                "deny_write_tools": deny_write_tools,
                "deny_shell_tools": deny_shell_tools,
                "execution_context": execution_context
                    .as_ref()
                    .map(SessionExecutionContext::audit_summary),
            }),
            Self::WorkOnProject {
                project,
                client_id,
                path,
                mode,
                base_ref,
                instruction,
                include_project_instructions,
                include_workflow_guidance,
                session_id,
            } => serde_json::json!({
                "project": project,
                "client_id": client_id,
                "path_source_requested": path.is_some(),
                "mode": mode,
                "base_ref_present": base_ref.is_some(),
                "instruction_present": true,
                "instruction_summary": command_preview(instruction),
                "include_project_instructions": include_project_instructions,
                "include_workflow_guidance": include_workflow_guidance,
                "session_id": session_id,
            }),
            Self::UpdateSessionContext {
                project,
                session_id,
                execution_context,
            } => serde_json::json!({
                "project": project,
                "session_id": session_id,
                "execution_context": execution_context.audit_summary(),
            }),
            Self::FinishCodingTask {
                project,
                session_id,
                summary_only,
                include_diff,
                include_workspace,
                include_hygiene,
                include_handoff,
                include_validation_summary,
            } => serde_json::json!({
                "project": project,
                "session_id": session_id,
                "summary_only": summary_only,
                "include_diff": include_diff,
                "include_workspace": include_workspace,
                "include_hygiene": include_hygiene,
                "include_handoff": include_handoff,
                "include_validation_summary": include_validation_summary,
            }),
            Self::ListProjects {
                client_id,
                project,
                query,
                limit,
                summary_only,
            } => serde_json::json!({
                "client_id_present": client_id.is_some(),
                "project_present": project.is_some(),
                "query_present": query.is_some(),
                "query_length": query.as_deref().map(|value| value.chars().count()).unwrap_or_default(),
                "limit": limit,
                "summary_only": summary_only,
            }),
            Self::ListRunners {
                client_id,
                client_ids,
                include_projects,
                summary_only,
            } => serde_json::json!({
                "client_id_present": client_id.is_some(),
                "client_ids_count": client_ids.as_ref().map(Vec::len).unwrap_or_default(),
                "include_projects": include_projects,
                "summary_only": summary_only,
            }),
            Self::ListJobs {
                limit,
                status,
                project,
                session_id,
            } => serde_json::json!({
                "limit": limit,
                "status": status,
                "project_present": project.is_some(),
                "session_id_present": session_id.is_some(),
            }),
            Self::ToolManifest {
                tool_name,
                category,
                intent,
                include_recommended_flows,
                include_risk_summary,
            } => serde_json::json!({
                "tool_name": tool_name,
                "category": category,
                "intent": intent,
                "include_recommended_flows": include_recommended_flows,
                "include_risk_summary": include_risk_summary,
            }),
            Self::ListTools {
                category,
                features,
                summary_only,
                limit,
            } => serde_json::json!({
                "category": category,
                "features": features,
                "summary_only": summary_only,
                "limit": limit,
            }),
            Self::SessionSummary { session_id, limit } => serde_json::json!({
                "session_id": session_id,
                "limit": limit,
            }),
            Self::CloseSession { session_id } => serde_json::json!({
                "session_id": session_id,
            }),
            Self::ValidationSummary {
                project,
                session_id,
                limit,
            } => serde_json::json!({
                "project": project,
                "session_id": session_id,
                "limit": limit,
            }),
            Self::GetSessionAssignment {
                session_id,
                message_id,
            } => serde_json::json!({
                "session_id": session_id,
                "message_id": message_id,
            }),
            Self::RunDetachedProcess {
                project,
                executable: _,
                args,
                stdin,
                timeout_secs,
                cwd,
                purpose,
                ..
            } => serde_json::json!({
                "project": project,
                "executable_present": true,
                "stdin_present": stdin.is_some(),
                "arg_count": args.len(),
                "process_summary": format!("detached process ({} args)", args.len()),
                "timeout_secs": timeout_secs,
                "cwd": cwd,
                "purpose": purpose,
            }),
            Self::JobStatus {
                job_id,
                include_command_preview,
            } => serde_json::json!({
                "job_id": job_id,
                "include_command_preview": include_command_preview,
            }),
            Self::JobLog {
                job_id,
                offset,
                tail_lines,
                after_observation_token,
                wait_secs,
            } => serde_json::json!({
                "job_id": job_id,
                "offset": offset,
                "tail_lines": tail_lines,
                "token_present": after_observation_token.is_some(),
                "wait_secs": wait_secs,
            }),
            Self::JobTail {
                job_id,
                tail_lines,
                after_observation_token,
                wait_secs,
            } => serde_json::json!({
                "job_id": job_id,
                "tail_lines": tail_lines,
                "token_present": after_observation_token.is_some(),
                "wait_secs": wait_secs,
            }),
            Self::ListProjectTrackedFiles {
                project,
                path,
                globs,
                depth,
                limit,
                offset,
                ..
            } => serde_json::json!({
                "project": project,
                "path": path,
                "globs": globs,
                "depth": depth,
                "limit": limit,
                "offset": offset,
            }),
            Self::ExportProjectArtifact { project, path, .. } => serde_json::json!({
                "project": project,
                "path": path,
            }),
            Self::ComputerListDisplays { client_id, limit } => serde_json::json!({
                "client_id": client_id,
                "limit": limit,
            }),
            Self::ComputerReadClipboard { client_id } => serde_json::json!({
                "client_id": client_id,
            }),
            Self::ComputerWriteClipboard { client_id, text } => serde_json::json!({
                "client_id": client_id,
                "text_bytes": text.len(),
            }),
            Self::ComputerPointerMove {
                client_id,
                display_id,
                snapshot_generation,
                x,
                y,
            }
            | Self::ComputerPointerClick {
                client_id,
                display_id,
                snapshot_generation,
                x,
                y,
            } => serde_json::json!({
                "client_id": client_id,
                "display_id": display_id,
                "snapshot_generation": snapshot_generation,
                "x": x,
                "y": y,
            }),
            Self::ComputerSnapshotDisplay {
                client_id,
                display_id,
                max_width,
                max_height,
            } => serde_json::json!({
                "client_id": client_id,
                "display_id": display_id,
                "max_width": max_width,
                "max_height": max_height,
            }),
            Self::RegisterProject {
                client_id,
                id,
                name,
                path,
                description,
                allow_patch,
                overwrite,
            } => serde_json::json!({
                "client_id": client_id,
                "id": id,
                "name": name,
                "path_present": !path.is_empty(),
                "description_present": description.is_some(),
                "description_bytes": description.as_ref().map(String::len).unwrap_or_default(),
                "allow_patch": allow_patch,
                "overwrite": overwrite,
            }),
            Self::UnregisterProject {
                project,
                expected_revision,
            } => serde_json::json!({
                "project": project,
                "expected_revision": expected_revision,
            }),
            Self::CreateProject {
                client_id,
                id,
                name,
                path,
                description,
                allow_patch,
                template,
                git_init,
                adopt_existing_empty,
                overwrite,
            } => serde_json::json!({
                "client_id": client_id,
                "id": id,
                "name": name,
                "path_present": !path.is_empty(),
                "description_present": description.is_some(),
                "description_bytes": description.as_ref().map(String::len).unwrap_or_default(),
                "allow_patch": allow_patch,
                "template": template,
                "git_init": git_init,
                "adopt_existing_empty": adopt_existing_empty,
                "overwrite": overwrite,
            }),
            Self::RunnerConfigCheck { client_id } => serde_json::json!({
                "client_id": client_id,
            }),
            Self::RunnerConfigReload {
                client_id,
                expected_generation,
            } => serde_json::json!({
                "client_id": client_id,
                "expected_generation": expected_generation,
            }),
            Self::PluginTool(plugin) => serde_json::json!({
                "action": plugin.action,
                "runner_present": plugin.runner.is_some(),
                "plugin_present": plugin.plugin.is_some(),
                "tool_present": plugin.tool.is_some(),
                "binding_present": plugin.binding.is_some(),
                "arguments_present": plugin.arguments.is_some(),
                "argument_key_count": plugin.arguments.as_ref().and_then(Value::as_object).map(serde_json::Map::len).unwrap_or_default(),
            }),
            Self::SshResource(resource) => serde_json::json!({
                "action": resource.action,
                "runner_present": resource.runner.is_some(),
                "binding_present": resource.binding.is_some(),
                "name_present": resource.name.is_some(),
                "target_present": resource.target.is_some(),
                "default_cwd_present": resource.default_cwd.is_some(),
            }),
            Self::ReadToolTrace {
                trace_ref,
                offset,
                limit,
                payload_index,
            } => serde_json::json!({
                "trace_ref": trace_ref,
                "offset": offset,
                "limit": limit,
                "payload_index": payload_index,
            }),
            Self::RuntimeStatus {
                compact,
                summary_only,
                client_id,
            } => serde_json::json!({
                "compact": compact,
                "summary_only": summary_only,
                "client_id_present": client_id.is_some(),
            }),
            Self::WorkspaceHygieneCheck {
                project,
                max_findings,
                include_tracked,
                ..
            } => serde_json::json!({
                "project": project,
                "max_findings": max_findings,
                "include_tracked": include_tracked,
            }),
        }
    }
}
