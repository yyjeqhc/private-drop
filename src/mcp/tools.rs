use super::presentation;
use super::resources;
use super::response::{
    connector_call_tool_result, mcp_runtime_tool_result_fallback, mcp_stateless_result, rpc_error,
    rpc_result,
};
use super::tasks;
use super::{require_mcp_scope, scope_forbidden, McpOutcome};
use crate::auth::AuthContext;
use crate::connector_runtime::{ConnectorRuntime, ConnectorTransport};
pub(super) use crate::model_surface::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME;
use crate::model_surface::{ModelSurface, RuntimeExposure};
use crate::tool_request_trace::ToolRequestLifecycle;
use crate::tool_runtime::kernel::{
    check_runtime_tool_scope, HostFileImportTrust, ToolCallContext, ToolCallErrorStatus,
    ToolCallRequest as KernelToolCallRequest, ToolInvocationMetadata, ToolProtocolCapabilities,
    ToolTransport,
};
use crate::tool_runtime::model_ergonomics_telemetry::{
    ModelErgonomicsRecord, ModelErgonomicsTimer,
};
use crate::tool_runtime::specialized::SpecializedGovernanceDenial;
use crate::tool_runtime::tool_definition::{
    is_adaptive_runtime_direct_tool, runtime_tool_accepts_context_ack, LOCAL_CODING_TOOL_NAMES,
};
use crate::tool_runtime::{registered_tool_specs, ToolCall, ToolResult, ToolRuntime, ToolSpec};
use serde::Deserialize;
use serde_json::{json, Value};

fn filter_specs_for_oauth(mut specs: Vec<ToolSpec>, auth: Option<&AuthContext>) -> Vec<ToolSpec> {
    let oauth_scope_projection = auth.is_some_and(AuthContext::is_oauth_token);
    specs.retain(|spec| {
        let authority = crate::tool_runtime::metadata::lookup_tool_metadata(&spec.name)
            .map(|metadata| metadata.authority);
        matches!(
            authority,
            Some(webcodex_core::authority::ToolAuthorityPolicy::RequireAny(_))
        )
        .then(|| check_runtime_tool_scope(auth, &spec.name).is_ok())
        .unwrap_or_else(|| {
            !oauth_scope_projection || check_runtime_tool_scope(auth, &spec.name).is_ok()
        })
    });
    specs
}

fn full_operator_runtime_specs_for_auth(
    stateless_2026: bool,
    auth: Option<&AuthContext>,
) -> Vec<ToolSpec> {
    let oauth_scope_projection = auth.is_some_and(AuthContext::is_oauth_token);
    let mut specs = registered_tool_specs();
    specs.retain(|spec| {
        let authority = crate::tool_runtime::metadata::lookup_tool_metadata(&spec.name)
            .map(|metadata| metadata.authority);
        matches!(
            authority,
            Some(webcodex_core::authority::ToolAuthorityPolicy::RequireAny(_))
        )
        .then(|| check_runtime_tool_scope(auth, &spec.name).is_ok())
        .unwrap_or_else(|| {
            !oauth_scope_projection || check_runtime_tool_scope(auth, &spec.name).is_ok()
        })
    });
    if stateless_2026 {
        specs.extend(
            crate::tool_runtime::stateless_operator_extension_tool_specs()
                .into_iter()
                .filter(|spec| {
                    if crate::tool_runtime::skills::is_skill_runtime_tool_name(&spec.name) {
                        !oauth_scope_projection
                            || check_runtime_tool_scope(auth, &spec.name).is_ok()
                    } else if crate::tool_runtime::skills::is_skill_management_tool_name(&spec.name)
                    {
                        auth.is_some_and(|auth| auth.has_scope(crate::auth::SCOPE_ADMIN))
                    } else {
                        check_runtime_tool_scope(auth, &spec.name).is_ok()
                    }
                }),
        );
    }
    specs
}

fn adaptive_runtime_gateway_tool_spec() -> ToolSpec {
    ToolSpec {
        name: ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME.to_string(),
        description: "Call one runtime tool admitted on the adaptive surface through the generic gateway. A tool with availability=direct should still be invoked through its direct callable when available, but call_runtime_tool is an allowed fallback when that callable is unavailable or not loaded. Directness changes preferred model exposure only: canonical runtime argument validation, OAuth scope, Project authority, permission gates, Runner capability, Session/context policy, host-file-import trust, protocol capability admission, and tool effects remain unchanged.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "tool": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 128,
                    "description": "Exact runtime tool name admitted on the adaptive surface. availability=direct is the preferred route, but the same admitted target may use this gateway as a fallback when the direct callable is unavailable or not loaded."
                },
                "arguments": {
                    "type": "object",
                    "description": "Arguments for the selected runtime tool. Discover its contract before calling when it is not already known.",
                    "additionalProperties": true
                }
            },
            "required": ["tool", "arguments"],
            "additionalProperties": false
        }),
        output_schema: json!({
            "type": "object",
            "description": "Normal MCP result for the selected runtime tool.",
            "additionalProperties": true
        }),
        annotations: json!({
            "readOnlyHint": false,
            "destructiveHint": true,
            "idempotentHint": false,
            "openWorldHint": false
        }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdaptiveRuntimeGatewayTargetRoute {
    Gateway,
    Direct,
    Recursive,
    Unknown,
}

fn adaptive_runtime_gateway_target_route(
    target: &str,
    stateless_2026: bool,
) -> AdaptiveRuntimeGatewayTargetRoute {
    if target == ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME {
        return AdaptiveRuntimeGatewayTargetRoute::Recursive;
    }
    let operator_extension_admitted = stateless_2026
        && crate::tool_runtime::stateless_operator_extension_tool_specs()
            .iter()
            .any(|spec| spec.name == target);
    let (availability, gateway_tool) = if operator_extension_admitted {
        ModelSurface::AdaptiveRuntime
            .runtime_tool_invocation_route_with_operator_extension(target, true)
    } else {
        ModelSurface::AdaptiveRuntime.runtime_tool_invocation_route(target)
    };
    match (availability, gateway_tool) {
        (crate::model_surface::TOOL_SURFACE_AVAILABILITY_DIRECT, None) => {
            return AdaptiveRuntimeGatewayTargetRoute::Direct;
        }
        (
            crate::model_surface::TOOL_SURFACE_AVAILABILITY_GATEWAY,
            Some(ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME),
        ) => {
            return AdaptiveRuntimeGatewayTargetRoute::Gateway;
        }
        _ => {}
    }
    // The MCP adapter's own gateway remains a specialized protocol route. It is
    // not part of the runtime ToolSpec extension universe.
    if target == crate::mcp_gateway::MCP_TOOL_NAME {
        return AdaptiveRuntimeGatewayTargetRoute::Gateway;
    }
    AdaptiveRuntimeGatewayTargetRoute::Unknown
}

#[cfg(test)]
pub(crate) fn adaptive_runtime_gateway_target_admitted_for_test(
    target: &str,
    stateless_2026: bool,
) -> bool {
    matches!(
        adaptive_runtime_gateway_target_route(target, stateless_2026),
        AdaptiveRuntimeGatewayTargetRoute::Gateway | AdaptiveRuntimeGatewayTargetRoute::Direct
    )
}

fn unwrap_adaptive_runtime_gateway_arguments(
    arguments: Value,
    stateless_2026: bool,
) -> Result<(String, Value), String> {
    let mut outer = arguments
        .as_object()
        .cloned()
        .ok_or_else(|| "adaptive runtime gateway arguments must be an object".to_string())?;
    let target = outer
        .remove("tool")
        .and_then(|value| value.as_str().map(str::trim).map(str::to_string))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "adaptive runtime gateway field 'tool' must be a non-empty string".to_string()
        })?;
    let mut target_arguments = outer.remove("arguments").unwrap_or_else(|| json!({}));
    if target_arguments.is_null() {
        target_arguments = json!({});
    }
    let target_object = target_arguments.as_object_mut().ok_or_else(|| {
        "adaptive runtime gateway field 'arguments' must be an object".to_string()
    })?;

    let mut allowed_wrapper_fields = Vec::new();
    if stateless_2026 {
        allowed_wrapper_fields.extend([
            crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD,
            crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD,
            crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD,
            crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD,
            crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD,
        ]);
    }
    for (key, value) in outer {
        if !allowed_wrapper_fields.contains(&key.as_str()) {
            return Err(format!(
                "unsupported adaptive runtime gateway field '{key}'"
            ));
        }
        if target_object.insert(key.clone(), value).is_some() {
            return Err(format!(
                "adaptive runtime gateway field '{key}' was supplied both outside and inside 'arguments'"
            ));
        }
    }
    Ok((target, target_arguments))
}

fn adaptive_runtime_gateway_unknown_target(target: &str) -> ToolResult {
    ToolResult::err_with_output(
        format!("unknown adaptive runtime tool '{target}'"),
        json!({
            "error_kind": "unknown_tool",
            "execution_state": "not_started",
            "state_changed": false,
            "target_tool": target,
            "recovery_kind": "fix_input"
        }),
    )
}

#[derive(Debug, Deserialize)]
pub(super) struct McpToolCallParams {
    pub(super) name: String,
    #[serde(default)]
    pub(super) arguments: Value,
}

pub(super) fn tool_name_from_params(params: &Value) -> Option<String> {
    let name = params.get("name").and_then(Value::as_str)?;
    if name == ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME {
        return params["arguments"]["tool"]
            .as_str()
            .map(str::to_string)
            .or_else(|| Some(name.to_string()));
    }
    Some(name.to_string())
}

pub(super) fn project_from_tool_call_params(params: &Value) -> Option<String> {
    if params.get("name").and_then(Value::as_str) == Some(ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME) {
        return params["arguments"]["arguments"]["project"]
            .as_str()
            .map(str::to_string);
    }
    params["arguments"]["project"].as_str().map(str::to_string)
}

/// Test helper for legacy/non-stateless tools/list rendering. Production uses
/// the canonical auth/surface renderer below directly.
#[cfg(test)]
pub(super) fn mcp_tools_list_payload_with_compact(
    model_surface: ModelSurface,
    compact: bool,
) -> Value {
    mcp_tools_list_payload_with_features(model_surface, compact, false, false)
}

#[cfg(test)]
pub(super) fn mcp_tools_list_payload_with_compact_and_app(
    model_surface: ModelSurface,
    compact: bool,
    app_enabled: bool,
) -> Value {
    mcp_tools_list_payload_with_features(model_surface, compact, app_enabled, true)
}

#[cfg(test)]
fn mcp_tools_list_payload_with_features(
    model_surface: ModelSurface,
    compact: bool,
    app_enabled: bool,
    artifact_export_enabled: bool,
) -> Value {
    mcp_tools_list_payload_with_features_for_auth(
        model_surface,
        compact,
        app_enabled,
        artifact_export_enabled,
        false,
        None,
    )
}

pub(super) fn mcp_tools_list_payload_with_features_for_auth(
    model_surface: ModelSurface,
    compact: bool,
    app_enabled: bool,
    artifact_export_enabled: bool,
    stateless_2026: bool,
    auth: Option<&AuthContext>,
) -> Value {
    let specs = match model_surface {
        ModelSurface::LocalCoding => {
            filter_specs_for_oauth(crate::model_surface::local_coding_tool_specs(), auth)
        }
        ModelSurface::AdaptiveRuntime => filter_specs_for_oauth(
            crate::model_surface::adaptive_runtime_direct_tool_specs(),
            auth,
        ),
        ModelSurface::FullOperatorRuntime => {
            full_operator_runtime_specs_for_auth(stateless_2026, auth)
        }
    };

    let mut tools = specs
        .into_iter()
        .filter(|spec| artifact_export_enabled || spec.name != "export_project_artifact")
        .map(|spec| mcp_tool_spec_json(spec, compact, app_enabled))
        .collect::<Vec<_>>();
    if app_enabled && stateless_2026 && model_surface.supports_operator_extensions() {
        let mut app_specs = filter_specs_for_oauth(
            crate::tool_runtime::goal_plan_app_tool_specs()
                .into_iter()
                .chain(crate::tool_runtime::agent_continuation_app_tool_specs())
                .collect(),
            auth,
        )
        .into_iter()
        .map(|spec| {
            let mut value = mcp_tool_spec_json(spec, compact, false);
            attach_app_visibility(&mut value);
            value
        })
        .collect::<Vec<_>>();
        tools.append(&mut app_specs);
    }
    json!({ "tools": tools })
}

fn project_connector_tools_list_payload_for_auth(
    compact: bool,
    auth: Option<&AuthContext>,
) -> Value {
    let tools = filter_specs_for_oauth(crate::connector_runtime::surface::capability_specs(), auth)
        .into_iter()
        .map(|spec| mcp_tool_spec_json(spec, compact, false))
        .collect::<Vec<_>>();
    json!({ "tools": tools })
}

#[cfg(test)]
pub(super) fn project_connector_tools_list_payload_with_compact(compact: bool) -> Value {
    project_connector_tools_list_payload_for_auth(compact, None)
}

fn adapt_computer_snapshot_output_schema_for_mcp(spec: &mut ToolSpec) {
    let properties = spec
        .output_schema
        .pointer_mut("/properties/output/properties")
        .and_then(Value::as_object_mut)
        .expect("computer_snapshot output schema properties");
    properties.remove("content_base64");
    properties.insert(
        "content_delivery".to_string(),
        json!({
            "type": "string",
            "const": "mcp_image",
            "description": "MCP native-image delivery marker; binary image bytes are carried in the image ContentBlock rather than structuredContent."
        }),
    );
}

fn mcp_context_projection_output_schema() -> Value {
    json!({
        "type": "object",
        "description": "Optional bounded post-tool context sidecar. It describes material projected after the main effect/observation and never grants authority or retroactively governs that effect.",
        "properties": {
            "timing": {"type": "string", "const": "post_tool"},
            "applies_to_current_effect": {"type": "boolean", "const": false},
            "materials": {
                "type": "array",
                "maxItems": crate::tool_runtime::context_projection::MAX_CONTEXT_REQUEST_ITEMS,
                "items": {
                    "type": "object",
                    "properties": {
                        "key": {"type": "string", "maxLength": crate::tool_runtime::context_projection::MAX_CONTEXT_REQUEST_KEY_CHARS},
                        "status": {"type": "string", "enum": ["available", "unavailable", "unsupported"]},
                        "reason_code": {"type": "string"},
                        "projection": {}
                    },
                    "required": ["key", "status"],
                    "additionalProperties": false
                }
            },
            "truncated": {"type": "boolean"}
        },
        "required": ["timing", "applies_to_current_effect", "materials", "truncated"],
        "additionalProperties": false
    })
}

fn add_context_projection_to_output_shape(schema: &mut Value, projection_schema: &Value) {
    if schema.get("type").and_then(Value::as_str) == Some("object") {
        if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            properties.insert("context_projection".to_string(), projection_schema.clone());
        }
    }
    for keyword in ["anyOf", "oneOf", "allOf"] {
        if let Some(branches) = schema.get_mut(keyword).and_then(Value::as_array_mut) {
            for branch in branches {
                add_context_projection_to_output_shape(branch, projection_schema);
            }
        }
    }
}

fn add_stateless_context_projection_output_schema(tool: &mut Value) {
    let Some(output_schema) = tool.get_mut("outputSchema") else {
        return;
    };
    let projection_schema = mcp_context_projection_output_schema();
    if let Some(output) = output_schema.pointer_mut("/properties/output") {
        add_context_projection_to_output_shape(output, &projection_schema);
    }
    if let Some(conditions) = output_schema.get_mut("allOf").and_then(Value::as_array_mut) {
        for condition in conditions {
            for branch_name in ["then", "else"] {
                if let Some(output) =
                    condition.pointer_mut(&format!("/{branch_name}/properties/output"))
                {
                    add_context_projection_to_output_shape(output, &projection_schema);
                }
            }
        }
    }
}

pub(super) fn add_stateless_workflow_recorder_metadata(
    payload: &mut Value,
    model_surface: ModelSurface,
) {
    let Some(tools) = payload.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools {
        let tool_name = tool.get("name").and_then(Value::as_str);
        if tool_name == Some("goal_plan_state")
            || tool_name.is_some_and(is_agent_continuation_app_tool_name)
        {
            continue;
        }
        let accepts_context_ack = tool
            .get("name")
            .and_then(Value::as_str)
            .is_none_or(runtime_tool_accepts_context_ack);
        let Some(properties) = tool
            .pointer_mut("/inputSchema/properties")
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        properties.insert(
            crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD.to_string(),
            json!({
                "type": "string",
                "pattern": "^wc_sess_[A-Za-z0-9_]+$",
                "description": "Optional explicit Workflow Session used only to record this call and trusted collaboration provenance. Separate from any tool business Session input; grants no authority; removed before concrete parsing."
            }),
        );
        properties.insert(
            crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD.to_string(),
            json!({
                "type": "array",
                "maxItems": crate::tool_runtime::sessions::MAX_TOOL_CALL_ACK_MESSAGE_IDS,
                "items": {
                    "type": "string",
                    "pattern": "^wc_msg_[A-Za-z0-9_]+$"
                },
                "description": "Proves the current model context still retains the listed open ACK-required Session messages. Repeat while retained. If later omitted, unresolved ACK-required guidance may be surfaced again. ACK neither resolves messages nor grants authority or gates execution."
            }),
        );
        properties.insert(
            crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD.to_string(),
            json!({
                "type": "object",
                "description": "After handling one non-todo message in the explicit recording Session, attach its id and bounded resolution text here to resolve it on the same WebCodex call. ACK-required guidance also needs request-scoped ACK. Applies only to that exact recording Session; removed before concrete parsing; does not predict call success. Todos use the atomic completion path.",
                "properties": {
                    "message_id": {
                        "type": "string",
                        "pattern": "^wc_msg_[A-Za-z0-9_]+$"
                    },
                    "resolution": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": crate::tool_runtime::sessions::MAX_MESSAGE_RESOLUTION_CHARS
                    }
                },
                "required": ["message_id", "resolution"],
                "additionalProperties": false
            }),
        );
        if model_surface.supports_operator_extensions() {
            properties.insert(
                crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD
                    .to_string(),
                json!({
                    "type": "array",
                    "maxItems": crate::tool_runtime::context_projection::MAX_CONTEXT_REQUEST_ITEMS,
                    "items": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": crate::tool_runtime::context_projection::MAX_CONTEXT_REQUEST_KEY_CHARS,
                        "description": "Bounded context material key; unsupported keys are reported nonfatally."
                    },
                    "description": format!("Request bounded context material after this tool's main effect/observation; keys are open-ended and currently include {}. This sidecar grants no authority and cannot make requested guidance a retroactive precondition of the current effect. Recover missing project or Memory guidance on a read/observation call before any later dependent mutation.", crate::tool_runtime::context_projection::context_material_keys_csv())
                }),
            );
            if accepts_context_ack {
                properties.insert(
                    crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD
                        .to_string(),
                    json!({
                        "type": "integer",
                        "minimum": 0,
                        "description": "Echo the latest Session context revision retained by the model; omit it when unknown. A known behind revision may receive bounded delta recovery; missing, invalid, future, lost-history, or truncated recovery gets a compact current Session handoff. Recovery is nonblocking."
                    }),
                );
            }
            add_stateless_context_projection_output_schema(tool);
        }
    }
}

fn tool_meta_object(value: &mut Value) -> Option<&mut serde_json::Map<String, Value>> {
    let object = value.as_object_mut()?;
    let meta = object
        .entry("_meta".to_string())
        .or_insert_with(|| json!({}));
    meta.as_object_mut()
}

pub(super) fn attach_app_metadata(value: &mut Value, resource_uri: &str) {
    let Some(meta) = tool_meta_object(value) else {
        return;
    };
    let ui = meta.entry("ui".to_string()).or_insert_with(|| json!({}));
    let Some(ui) = ui.as_object_mut() else {
        return;
    };
    ui.insert(
        "resourceUri".to_string(),
        Value::String(resource_uri.to_string()),
    );
}

fn attach_app_visibility(value: &mut Value) {
    let Some(meta) = tool_meta_object(value) else {
        return;
    };
    let ui = meta.entry("ui".to_string()).or_insert_with(|| json!({}));
    let Some(ui) = ui.as_object_mut() else {
        return;
    };
    ui.insert("visibility".to_string(), json!(["app"]));
}

fn is_agent_continuation_app_tool_name(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "agent_continuation_bind"
            | "agent_continuation_state"
            | "agent_continuation_wake_acquire"
            | "agent_continuation_wake_prepare"
            | "agent_continuation_wake_finish"
            | "agent_continuation_unbind"
    )
}

fn attach_app_tool_content_fallback(result: &mut Value) {
    let Some(structured) = result.get("structuredContent") else {
        return;
    };
    let Ok(text) = serde_json::to_string(structured) else {
        return;
    };
    result["content"] = json!([{ "type": "text", "text": text }]);
}

fn mcp_tool_spec_json(mut spec: ToolSpec, compact: bool, app_enabled: bool) -> Value {
    let tool_name = spec.name.clone();
    if matches!(
        tool_name.as_str(),
        "computer_snapshot" | "computer_snapshot_display"
    ) {
        adapt_computer_snapshot_output_schema_for_mcp(&mut spec);
    }
    if tool_name == "read_project_artifact" {
        if let Some(properties) = spec.input_schema["properties"].as_object_mut() {
            properties.insert(
                "as_image".to_string(),
                json!({
                    "type": "boolean",
                    "description": "MCP-only. When true, read one complete PNG, JPEG, or WebP up to 1 MiB and return it as native image content. Cannot be combined with offset or length."
                }),
            );
        }
        spec.description.push_str(
            " Over MCP, set as_image=true to return one complete PNG, JPEG, or WebP as native image content; ordinary calls keep the existing chunked base64 response.",
        );
    }
    let mut value = if compact {
        json!({
            "name": spec.name,
            "description": spec.description,
            "inputSchema": spec.input_schema,
            "annotations": spec.annotations,
        })
    } else {
        // Match ToolSpec's camelCase serde so default behavior is unchanged.
        serde_json::to_value(spec).unwrap_or_else(|_| json!({}))
    };
    if !compact && tool_name == crate::ssh_resource_gateway::SSH_RESOURCE_TOOL_NAME {
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "outputSchema".to_string(),
                crate::ssh_resource_gateway::mcp_output_schema(),
            );
        }
    }
    if tool_name == "import_conversation_files_to_project" {
        if let Some(meta) = tool_meta_object(&mut value) {
            meta.insert("openai/fileParams".to_string(), json!(["openaiFileIdRefs"]));
        }
    }
    if app_enabled && presentation::tool_supports_result_app(&tool_name) {
        attach_app_metadata(&mut value, resources::MCP_RESULT_UI_RESOURCE_URI);
    }
    if app_enabled && presentation::tool_supports_goal_plan_app(&tool_name) {
        attach_app_metadata(&mut value, resources::MCP_GOAL_PLAN_UI_RESOURCE_URI);
    }
    if app_enabled && presentation::tool_supports_agent_continuation_app(&tool_name) {
        attach_app_metadata(
            &mut value,
            resources::MCP_AGENT_CONTINUATION_UI_RESOURCE_URI,
        );
    }
    value
}

#[cfg(test)]
pub(super) fn mcp_runtime_tool_result(
    tool_name: &str,
    as_image_requested: bool,
    result: ToolResult,
) -> Value {
    if tool_name == "export_project_artifact" {
        return mcp_runtime_tool_result_fallback(result);
    }
    match resources::adapt_tool_result(
        tool_name,
        as_image_requested,
        result,
        resources::McpResourceToolCallContext::default(),
    ) {
        resources::McpResourceToolResultAdaptation::Framed(value) => value,
        resources::McpResourceToolResultAdaptation::Unhandled(result) => {
            mcp_runtime_tool_result_fallback(result)
        }
    }
}

pub(super) async fn handle_list(
    runtime: &ToolRuntime,
    id: Option<Value>,
    auth: Option<&AuthContext>,
    stateless_2026: bool,
    compact_schemas: bool,
    app_enabled: bool,
) -> McpOutcome {
    let result = match runtime.runtime_exposure() {
        RuntimeExposure::ProjectConnector => {
            project_connector_tools_list_payload_for_auth(compact_schemas, auth)
        }
        RuntimeExposure::Runtime(model_surface) => {
            let mut result = mcp_tools_list_payload_with_features_for_auth(
                model_surface,
                compact_schemas,
                app_enabled,
                stateless_2026,
                stateless_2026,
                auth,
            );
            if model_surface == ModelSurface::AdaptiveRuntime {
                if let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) {
                    tools.push(mcp_tool_spec_json(
                        adaptive_runtime_gateway_tool_spec(),
                        compact_schemas,
                        false,
                    ));
                }
            }
            if stateless_2026 {
                add_stateless_workflow_recorder_metadata(&mut result, model_surface);
            }
            if crate::mcp_gateway::authorized(auth) {
                if let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) {
                    tools.push(crate::mcp_gateway::tool_spec());
                }
            }
            if model_surface == ModelSurface::LocalCoding
                && crate::ssh_resource_gateway::authorized(auth)
            {
                if let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) {
                    tools.push(crate::ssh_resource_gateway::tool_spec(compact_schemas));
                }
            }
            result
        }
    };
    McpOutcome::Ok(rpc_result(
        id,
        if stateless_2026 {
            mcp_stateless_result(result, true)
        } else {
            result
        },
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostFileImportTrustReason {
    Trusted,
    MissingConfig,
    MissingDatabase,
    MissingAuth,
    NotOAuthToken,
    MissingAllowedClientId,
    OAuthDisabled,
    ClientIdNotConfigured,
    ClientRegistrationMissingOrRevoked,
    ClientRegistrationLookupFailed,
}

impl HostFileImportTrustReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::MissingConfig => "missing_config",
            Self::MissingDatabase => "missing_database",
            Self::MissingAuth => "missing_auth",
            Self::NotOAuthToken => "not_oauth_token",
            Self::MissingAllowedClientId => "missing_allowed_client_id",
            Self::OAuthDisabled => "oauth_disabled",
            Self::ClientIdNotConfigured => "client_id_not_configured",
            Self::ClientRegistrationMissingOrRevoked => "client_registration_missing_or_revoked",
            Self::ClientRegistrationLookupFailed => "client_registration_lookup_failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HostFileImportTrustDecision {
    pub(super) trust: HostFileImportTrust,
    pub(super) reason: HostFileImportTrustReason,
    config_present: bool,
    database_present: bool,
    oauth_enabled: bool,
    configured_trusted_client_count: usize,
    pub(super) client_id_configured: Option<bool>,
    pub(super) active_client_registration_found: Option<bool>,
}

#[cfg(test)]
static LAST_MCP_HOST_FILE_IMPORT_TRUST_DECISION: std::sync::OnceLock<
    std::sync::Mutex<Option<HostFileImportTrustDecision>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
pub(super) fn take_last_mcp_host_file_import_trust_decision() -> Option<HostFileImportTrustDecision>
{
    LAST_MCP_HOST_FILE_IMPORT_TRUST_DECISION
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap()
        .take()
}

impl HostFileImportTrustDecision {
    fn unavailable(reason: HostFileImportTrustReason) -> Self {
        Self {
            trust: HostFileImportTrust::Untrusted,
            reason,
            config_present: false,
            database_present: false,
            oauth_enabled: false,
            configured_trusted_client_count: 0,
            client_id_configured: None,
            active_client_registration_found: None,
        }
    }

    fn from_config(reason: HostFileImportTrustReason, config: &crate::Config) -> Self {
        Self {
            trust: HostFileImportTrust::Untrusted,
            reason,
            config_present: true,
            database_present: false,
            oauth_enabled: config.oauth2.enabled,
            configured_trusted_client_count: config.oauth2.trusted_mcp_file_client_ids.len(),
            client_id_configured: None,
            active_client_registration_found: None,
        }
    }
}

pub(super) fn mcp_host_file_import_trust_decision_from_state(
    config: &crate::Config,
    db: &crate::Database,
    auth: Option<&AuthContext>,
) -> HostFileImportTrustDecision {
    let base = HostFileImportTrustDecision {
        trust: HostFileImportTrust::Untrusted,
        reason: HostFileImportTrustReason::MissingAuth,
        config_present: true,
        database_present: true,
        oauth_enabled: config.oauth2.enabled,
        configured_trusted_client_count: config.oauth2.trusted_mcp_file_client_ids.len(),
        client_id_configured: None,
        active_client_registration_found: None,
    };
    let Some(auth) = auth else {
        return base;
    };
    if !auth.is_oauth_token() {
        return HostFileImportTrustDecision {
            reason: HostFileImportTrustReason::NotOAuthToken,
            ..base
        };
    }
    let Some(client_id) = auth
        .allowed_client_id
        .as_deref()
        .map(str::trim)
        .filter(|client_id| !client_id.is_empty())
    else {
        return HostFileImportTrustDecision {
            reason: HostFileImportTrustReason::MissingAllowedClientId,
            ..base
        };
    };
    if !config.oauth2.enabled {
        return HostFileImportTrustDecision {
            reason: HostFileImportTrustReason::OAuthDisabled,
            ..base
        };
    }
    let client_id_configured = config
        .oauth2
        .trusted_mcp_file_client_ids
        .iter()
        .any(|trusted_client_id| trusted_client_id == client_id);
    if !client_id_configured {
        return HostFileImportTrustDecision {
            reason: HostFileImportTrustReason::ClientIdNotConfigured,
            client_id_configured: Some(false),
            ..base
        };
    }
    match db.get_oauth_client_by_client_id(client_id) {
        Ok(Some(client)) if client.client_id == client_id => HostFileImportTrustDecision {
            trust: HostFileImportTrust::TrustedOAuthClient,
            reason: HostFileImportTrustReason::Trusted,
            client_id_configured: Some(true),
            active_client_registration_found: Some(true),
            ..base
        },
        Ok(_) => HostFileImportTrustDecision {
            reason: HostFileImportTrustReason::ClientRegistrationMissingOrRevoked,
            client_id_configured: Some(true),
            active_client_registration_found: Some(false),
            ..base
        },
        Err(_) => HostFileImportTrustDecision {
            reason: HostFileImportTrustReason::ClientRegistrationLookupFailed,
            client_id_configured: Some(true),
            active_client_registration_found: None,
            ..base
        },
    }
}

#[cfg(test)]
pub(super) fn mcp_host_file_import_trust_from_state(
    config: &crate::Config,
    db: &crate::Database,
    auth: Option<&AuthContext>,
) -> HostFileImportTrust {
    mcp_host_file_import_trust_decision_from_state(config, db, auth).trust
}

pub(super) fn host_file_import_trust_for_call(
    tool_name: Option<&str>,
    auth: Option<&AuthContext>,
    config: Option<&crate::Config>,
    db: Option<&crate::Database>,
) -> HostFileImportTrust {
    if tool_name != Some("import_conversation_files_to_project") {
        return HostFileImportTrust::Untrusted;
    }
    let decision = match config {
        None => HostFileImportTrustDecision::unavailable(HostFileImportTrustReason::MissingConfig),
        Some(config) => match db {
            None => HostFileImportTrustDecision::from_config(
                HostFileImportTrustReason::MissingDatabase,
                config,
            ),
            Some(db) => mcp_host_file_import_trust_decision_from_state(config, db, auth),
        },
    };
    log_mcp_host_file_import_trust_decision(auth, &decision);
    decision.trust
}

fn mcp_auth_kind_classification(auth: Option<&AuthContext>) -> &'static str {
    match auth.map(|auth| auth.kind) {
        None => "none",
        Some(crate::auth::AuthKind::OAuth2Token) => "oauth2",
        Some(crate::auth::AuthKind::ApiToken) => "api_token",
        Some(crate::auth::AuthKind::Bootstrap) => "bootstrap",
        Some(crate::auth::AuthKind::AgentToken) => "agent_token",
        Some(crate::auth::AuthKind::AccountCredential) => "account_credential",
        Some(crate::auth::AuthKind::SharedKey) => "shared_key",
        Some(crate::auth::AuthKind::ProjectCredential) => "project_credential",
        Some(crate::auth::AuthKind::OpenAnonymous) => "open_anonymous",
    }
}

fn mcp_token_kind_classification(auth: Option<&AuthContext>) -> &'static str {
    match auth.and_then(|auth| auth.token_kind.as_deref()) {
        None => "none",
        Some("oauth2") => "oauth2",
        Some("oauth2_shared_key") => "oauth2_shared_key",
        Some("oauth2_project") => "oauth2_project",
        Some("user") => "user",
        Some("agent") => "agent",
        Some(_) => "other",
    }
}

fn log_mcp_host_file_import_trust_decision(
    auth: Option<&AuthContext>,
    decision: &HostFileImportTrustDecision,
) {
    #[cfg(test)]
    {
        *LAST_MCP_HOST_FILE_IMPORT_TRUST_DECISION
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .unwrap() = Some(*decision);
    }
    let allowed_client_id_present = auth
        .and_then(|auth| auth.allowed_client_id.as_deref())
        .is_some_and(|client_id| !client_id.trim().is_empty());
    tracing::info!(
        target: "webcodex::mcp",
        trust = decision.trust.is_trusted(),
        reason = decision.reason.as_str(),
        auth_kind = mcp_auth_kind_classification(auth),
        token_kind = mcp_token_kind_classification(auth),
        allowed_client_id_present,
        config_present = decision.config_present,
        database_present = decision.database_present,
        oauth_enabled = decision.oauth_enabled,
        configured_trusted_client_count = decision.configured_trusted_client_count,
        client_id_configured = ?decision.client_id_configured,
        active_client_registration_found = ?decision.active_client_registration_found,
        "mcp_host_file_import_trust_decision"
    );
}

pub(super) fn strip_recording_session_id(arguments: &mut Value) -> Result<Option<String>, String> {
    let Some(object) = arguments.as_object_mut() else {
        return Ok(None);
    };
    if object.contains_key("_session_id") {
        return Err(
            "field '_session_id' is no longer supported; use 'recording_session_id'".to_string(),
        );
    }
    match object.remove(crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD) {
        None => Ok(None),
        Some(Value::String(value)) => {
            let value = value.trim();
            if value.is_empty() {
                return Err(format!(
                    "field '{}' must be a non-empty string",
                    crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD
                ));
            }
            Ok(Some(value.to_string()))
        }
        Some(_) => Err(format!(
            "field '{}' must be a non-empty string",
            crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD
        )),
    }
}

pub(super) fn strip_stateless_ack_session_message_ids(
    arguments: &mut Value,
) -> Result<Vec<String>, String> {
    let Some(object) = arguments.as_object_mut() else {
        return Ok(Vec::new());
    };
    let Some(value) =
        object.remove(crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD)
    else {
        return Ok(Vec::new());
    };
    let Value::Array(values) = value else {
        return Err(format!(
            "field '{}' must be an array of wc_msg_* ids",
            crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD
        ));
    };
    if values.len() > crate::tool_runtime::sessions::MAX_TOOL_CALL_ACK_MESSAGE_IDS {
        return Err(format!(
            "field '{}' accepts at most {} message ids",
            crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD,
            crate::tool_runtime::sessions::MAX_TOOL_CALL_ACK_MESSAGE_IDS
        ));
    }
    let mut normalized = Vec::with_capacity(values.len());
    let mut seen = std::collections::HashSet::new();
    for value in values {
        let Value::String(value) = value else {
            return Err(format!(
                "field '{}' must contain only wc_msg_* strings",
                crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD
            ));
        };
        let value = value.trim();
        let valid = value.strip_prefix("wc_msg_").is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix
                    .as_bytes()
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        });
        if !valid {
            return Err(format!(
                "field '{}' must contain only valid wc_msg_* ids",
                crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD
            ));
        }
        if seen.insert(value.to_string()) {
            normalized.push(value.to_string());
        }
    }
    Ok(normalized)
}

pub(super) fn strip_stateless_session_message_resolution(
    arguments: &mut Value,
) -> Result<Option<crate::tool_runtime::sessions::ToolCallSessionMessageResolution>, String> {
    let Some(object) = arguments.as_object_mut() else {
        return Ok(None);
    };
    let Some(value) =
        object.remove(crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD)
    else {
        return Ok(None);
    };
    let Value::Object(mut fields) = value else {
        return Err(format!(
            "field '{}' must be an object with message_id and resolution",
            crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD
        ));
    };
    if fields.len() != 2 || !fields.contains_key("message_id") || !fields.contains_key("resolution")
    {
        return Err(format!(
            "field '{}' accepts exactly message_id and resolution",
            crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD
        ));
    }
    let Some(Value::String(message_id)) = fields.remove("message_id") else {
        return Err("session_message_resolution.message_id must be a wc_msg_* string".to_string());
    };
    let message_id = message_id.trim().to_string();
    let valid_message_id = message_id.strip_prefix("wc_msg_").is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    });
    if !valid_message_id {
        return Err(
            "session_message_resolution.message_id must be a valid wc_msg_* id".to_string(),
        );
    }
    let Some(Value::String(resolution)) = fields.remove("resolution") else {
        return Err("session_message_resolution.resolution must be a string".to_string());
    };
    let resolution = resolution.trim().to_string();
    if resolution.is_empty() {
        return Err("session_message_resolution.resolution must not be empty".to_string());
    }
    if resolution.chars().count() > crate::tool_runtime::sessions::MAX_MESSAGE_RESOLUTION_CHARS {
        return Err(format!(
            "session_message_resolution.resolution exceeds {} chars",
            crate::tool_runtime::sessions::MAX_MESSAGE_RESOLUTION_CHARS
        ));
    }
    Ok(Some(
        crate::tool_runtime::sessions::ToolCallSessionMessageResolution {
            message_id,
            resolution,
        },
    ))
}

pub(super) fn strip_stateless_context_request(
    arguments: &mut Value,
) -> Result<Vec<String>, String> {
    let Some(object) = arguments.as_object_mut() else {
        return Ok(Vec::new());
    };
    let Some(value) =
        object.remove(crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD)
    else {
        return Ok(Vec::new());
    };
    let Value::Array(values) = value else {
        return Err(format!(
            "field '{}' must be an array of bounded context material keys",
            crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD
        ));
    };
    if values.len() > crate::tool_runtime::context_projection::MAX_CONTEXT_REQUEST_ITEMS {
        return Err(format!(
            "field '{}' accepts at most {} context material keys",
            crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD,
            crate::tool_runtime::context_projection::MAX_CONTEXT_REQUEST_ITEMS
        ));
    }
    let mut normalized = Vec::with_capacity(values.len());
    let mut seen = std::collections::HashSet::new();
    for value in values {
        let Value::String(value) = value else {
            return Err(format!(
                "field '{}' must contain only strings",
                crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD
            ));
        };
        let key = value.trim();
        let valid = !key.is_empty()
            && key.chars().count()
                <= crate::tool_runtime::context_projection::MAX_CONTEXT_REQUEST_KEY_CHARS
            && key
                .chars()
                .all(|ch| !ch.is_control() && !ch.is_whitespace());
        if !valid {
            return Err(format!(
                "field '{}' keys must be non-empty, at most {} characters, and contain no whitespace or control characters",
                crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD,
                crate::tool_runtime::context_projection::MAX_CONTEXT_REQUEST_KEY_CHARS
            ));
        }
        if seen.insert(key.to_string()) {
            normalized.push(key.to_string());
        }
    }
    Ok(normalized)
}

pub(super) fn strip_stateless_ack_session_context_revision(arguments: &mut Value) -> Option<Value> {
    arguments
        .as_object_mut()?
        .remove(crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD)
}

pub(super) fn session_context_revision_ack_from_wire(
    value: Option<Value>,
) -> crate::tool_runtime::sessions::SessionContextRevisionAck {
    match value {
        None => crate::tool_runtime::sessions::SessionContextRevisionAck::Unacknowledged,
        Some(value) => value
            .as_u64()
            .map(crate::tool_runtime::sessions::SessionContextRevisionAck::Revision)
            .unwrap_or(crate::tool_runtime::sessions::SessionContextRevisionAck::Invalid),
    }
}

pub(super) async fn handle_call(
    runtime: &ToolRuntime,
    connector: Option<&ConnectorRuntime>,
    request_params: Value,
    id: Option<Value>,
    auth: Option<&AuthContext>,
    stateless_2026: bool,
    app_enabled: bool,
    server_mcp_apps_enabled: bool,
    host_file_import_trust: HostFileImportTrust,
    window: Option<&crate::client_window::ClientWindow>,
    mut lifecycle: Option<&mut ToolRequestLifecycle>,
    mut model_ergonomics_out: Option<&mut Option<ModelErgonomicsRecord>>,
    mut correlation_out: Option<&mut crate::tool_runtime::ToolCallCorrelation>,
) -> McpOutcome {
    let tasks_extension_declared = stateless_2026 && tasks::request_supports_tasks(&request_params);
    let mut params: McpToolCallParams = match serde_json::from_value(request_params) {
        Ok(params) => params,
        Err(e) => {
            return McpOutcome::BadRequest(rpc_error(id, -32602, format!("Invalid params: {}", e)));
        }
    };
    if runtime.runtime_exposure() == RuntimeExposure::ProjectConnector {
        if let Some(lc) = lifecycle.as_deref() {
            lc.capture_payload("raw_arguments", &params.arguments);
        }
    }
    if runtime.runtime_exposure() == RuntimeExposure::ProjectConnector {
        let connector = connector.expect("validated ProjectConnector runtime state");
        if !stateless_2026 && params.name == "task_start" && window.is_none() {
            if let Some(lc) = lifecycle.as_deref() {
                lc.dispatch_failed("window_identity_unavailable");
                lc.dispatch_finished(false, Some(false), "window_identity_unavailable");
            }
            return McpOutcome::BadRequest(rpc_error(
                id,
                -32600,
                "MCP session identity is unavailable; initialize the connection before starting or continuing project work",
            ));
        }
        if let Some(lc) = lifecycle.as_deref() {
            lc.capture_payload("effective_arguments", &params.arguments);
        }
        let task_polling = tasks_extension_declared
            && matches!(params.name.as_str(), "commands_run" | "checks_run");
        let outcome = if task_polling {
            connector
                .call_for_window_with_task_polling(
                    &params.name,
                    params.arguments,
                    auth,
                    ConnectorTransport::Mcp,
                    window,
                )
                .await
        } else {
            connector
                .call_for_window(
                    &params.name,
                    params.arguments,
                    auth,
                    ConnectorTransport::Mcp,
                    window,
                )
                .await
        };
        if let Some(required_scope) = outcome.required_scope {
            if let Some(lc) = lifecycle.as_deref() {
                lc.dispatch_failed("forbidden");
                lc.dispatch_finished(false, Some(false), "forbidden");
            }
            let description = outcome
                .body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("connector credential lacks the required scope")
                .to_string();
            return scope_forbidden(auth, Some(required_scope), description);
        }
        if outcome.protocol_error {
            if let Some(lc) = lifecycle.as_deref() {
                lc.dispatch_failed("invalid_arguments");
                lc.dispatch_finished(false, Some(false), "invalid_arguments");
            }
            let message = outcome
                .body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("invalid connector capability arguments")
                .to_string();
            return McpOutcome::BadRequest(rpc_error(id, -32602, message));
        }
        if let Some(lc) = lifecycle.as_deref() {
            let category = if outcome.ok { "success" } else { "tool_error" };
            lc.dispatch_finished(true, Some(outcome.ok), category);
        }
        if task_polling && outcome.ok {
            if let Some(task_outcome) =
                tasks::promote_connector_tool_call(&id, &outcome, auth, connector).await
            {
                return task_outcome;
            }
        }
        let result = connector_call_tool_result(outcome);
        return McpOutcome::Ok(rpc_result(
            id,
            if stateless_2026 {
                mcp_stateless_result(result, false)
            } else {
                result
            },
        ));
    }
    let RuntimeExposure::Runtime(model_surface) = runtime.runtime_exposure() else {
        unreachable!("ProjectConnector returned before runtime ModelSurface dispatch");
    };
    let via_adaptive_runtime_gateway = model_surface == ModelSurface::AdaptiveRuntime
        && params.name == ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME;
    if via_adaptive_runtime_gateway {
        let (target, arguments) =
            match unwrap_adaptive_runtime_gateway_arguments(params.arguments, stateless_2026) {
                Ok(target) => target,
                Err(message) => {
                    return McpOutcome::BadRequest(rpc_error(id, -32602, message));
                }
            };
        match adaptive_runtime_gateway_target_route(&target, stateless_2026) {
            AdaptiveRuntimeGatewayTargetRoute::Gateway
            | AdaptiveRuntimeGatewayTargetRoute::Direct => {
                params.name = target;
                params.arguments = arguments;
            }
            AdaptiveRuntimeGatewayTargetRoute::Unknown => {
                if let Some(lc) = lifecycle.as_deref() {
                    lc.dispatch_failed("unknown_tool");
                    lc.dispatch_finished(false, Some(false), "unknown_tool");
                }
                let rendered = mcp_runtime_tool_result_fallback(
                    adaptive_runtime_gateway_unknown_target(&target),
                );
                return McpOutcome::Ok(rpc_result(
                    id,
                    if stateless_2026 {
                        mcp_stateless_result(rendered, false)
                    } else {
                        rendered
                    },
                ));
            }
            AdaptiveRuntimeGatewayTargetRoute::Recursive => {
                return McpOutcome::BadRequest(rpc_error(
                    id,
                    -32602,
                    "call_runtime_tool cannot target itself through the adaptive runtime gateway",
                ));
            }
        }
    }
    if let Some(lc) = lifecycle.as_deref() {
        let audit = if params.name == crate::plugin_gateway::PLUGIN_TOOL_NAME {
            crate::plugin_gateway::audit_arguments(&params.arguments)
        } else if params.name == crate::ssh_resource_gateway::SSH_RESOURCE_TOOL_NAME {
            crate::ssh_resource_gateway::audit_arguments(&params.arguments)
        } else if via_adaptive_runtime_gateway {
            json!({"tool": params.name, "arguments_present": true})
        } else {
            params.arguments.clone()
        };
        lc.capture_payload("raw_arguments", &audit);
    }
    // Emit dispatch_started only after params parse succeeds and before
    // ToolRuntime work begins.
    if let Some(lc) = lifecycle.as_deref_mut() {
        lc.set_tool_name(Some(params.name.clone()));
        lc.dispatch_started();
    }
    if params.name == crate::mcp_gateway::MCP_TOOL_NAME {
        if let Some(outcome) = require_mcp_scope(auth, crate::auth::SCOPE_MCP_LOCAL) {
            if let Some(lc) = lifecycle.as_deref() {
                lc.dispatch_failed("forbidden");
                lc.dispatch_finished(false, Some(false), "forbidden");
            }
            return outcome;
        }
        if let Some(lc) = lifecycle.as_deref() {
            lc.capture_payload("effective_arguments", &params.arguments);
        }
        let result = crate::mcp_gateway::call(runtime, params.arguments, auth).await;
        let ok = result.get("isError").and_then(Value::as_bool) != Some(true);
        if let Some(lc) = lifecycle.as_deref() {
            lc.dispatch_finished(true, Some(ok), if ok { "success" } else { "tool_error" });
        }
        return McpOutcome::Ok(rpc_result(
            id,
            if stateless_2026 {
                mcp_stateless_result(result, false)
            } else {
                result
            },
        ));
    }
    if params.name == crate::plugin_gateway::PLUGIN_TOOL_NAME {
        let recording_session_id = match strip_recording_session_id(&mut params.arguments) {
            Ok(session_id) => session_id,
            Err(message) => {
                if let Some(lc) = lifecycle.as_deref() {
                    lc.dispatch_failed("invalid_arguments");
                    lc.dispatch_finished(false, Some(false), "invalid_arguments");
                }
                return McpOutcome::BadRequest(rpc_error(id, -32602, message));
            }
        };
        let call = match ToolCall::from_tool_name(&params.name, params.arguments.clone()) {
            Ok(call) => call,
            Err(message) => {
                if let Some(lc) = lifecycle.as_deref() {
                    lc.dispatch_failed("invalid_arguments");
                    lc.dispatch_finished(false, Some(false), "invalid_arguments");
                }
                return McpOutcome::BadRequest(rpc_error(id, -32602, message));
            }
        };
        let ToolCall::PluginTool(plugin) = call else {
            unreachable!("plugin_tool parser must yield ToolCall::PluginTool");
        };
        if let Some(lc) = lifecycle.as_deref() {
            lc.capture_payload(
                "effective_arguments",
                &crate::plugin_gateway::audit_arguments(&params.arguments),
            );
        }
        let invocation = match crate::plugin_gateway::invoke(
            runtime,
            plugin,
            recording_session_id.as_deref(),
            auth,
            crate::tool_runtime::sessions::SessionTransport::Mcp,
        )
        .await
        {
            Ok(invocation) => invocation,
            Err(SpecializedGovernanceDenial::Scope {
                required_scope,
                description,
            }) => {
                if let Some(lc) = lifecycle.as_deref() {
                    lc.dispatch_failed("forbidden");
                    lc.dispatch_finished(false, Some(false), "forbidden");
                }
                return scope_forbidden(auth, Some(required_scope), description);
            }
            Err(SpecializedGovernanceDenial::Tool(result)) => {
                if let Some(lc) = lifecycle.as_deref() {
                    lc.dispatch_failed("specialized_governance_denied");
                    lc.dispatch_finished(true, Some(false), "tool_error");
                }
                let result = mcp_runtime_tool_result_fallback(result);
                return McpOutcome::Ok(rpc_result(
                    id,
                    if stateless_2026 {
                        mcp_stateless_result(result, false)
                    } else {
                        result
                    },
                ));
            }
        };
        let ok = invocation.success();
        if let (Some(slot), Some(session_id)) = (
            correlation_out.as_deref_mut(),
            recording_session_id.as_deref(),
        ) {
            let mut correlation = crate::tool_runtime::ToolCallCorrelation::default();
            let project = runtime.sessions.session_project(session_id).flatten();
            correlation.resolved_project = project.clone();
            correlation.add_workflow_session(crate::tool_runtime::WorkflowSessionCorrelation {
                session_id: session_id.to_string(),
                project,
                relation: crate::tool_runtime::WorkflowSessionCorrelationRelation::Recording,
            });
            *slot = correlation;
        }
        if let Some(lc) = lifecycle.as_deref() {
            lc.capture_payload(
                "specialized_governance",
                &invocation.policy().audit_projection(),
            );
            lc.dispatch_finished(true, Some(ok), if ok { "success" } else { "tool_error" });
        }
        let result = invocation.to_mcp_result();
        return McpOutcome::Ok(rpc_result(
            id,
            if stateless_2026 {
                mcp_stateless_result(result, false)
            } else {
                result
            },
        ));
    }
    if params.name == crate::ssh_resource_gateway::SSH_RESOURCE_TOOL_NAME
        && (model_surface != ModelSurface::AdaptiveRuntime || via_adaptive_runtime_gateway)
    {
        let recording_session_id = match strip_recording_session_id(&mut params.arguments) {
            Ok(session_id) => session_id,
            Err(message) => {
                if let Some(lc) = lifecycle.as_deref() {
                    lc.dispatch_failed("invalid_arguments");
                    lc.dispatch_finished(false, Some(false), "invalid_arguments");
                }
                return McpOutcome::BadRequest(rpc_error(id, -32602, message));
            }
        };
        let policy = match crate::ssh_resource_gateway::operation_policy(&params.arguments) {
            Ok(policy) => policy,
            Err(_) => {
                if let Some(lc) = lifecycle.as_deref() {
                    lc.capture_payload(
                        "effective_arguments",
                        &crate::ssh_resource_gateway::audit_arguments(&params.arguments),
                    );
                }
                let result =
                    crate::ssh_resource_gateway::call(runtime, params.arguments, auth).await;
                let ok = result.get("isError").and_then(Value::as_bool) != Some(true);
                if let Some(lc) = lifecycle.as_deref() {
                    lc.dispatch_finished(true, Some(ok), if ok { "success" } else { "tool_error" });
                }
                return McpOutcome::Ok(rpc_result(
                    id,
                    if stateless_2026 {
                        mcp_stateless_result(result, false)
                    } else {
                        result
                    },
                ));
            }
        };
        let audit = crate::ssh_resource_gateway::audit_arguments(&params.arguments);
        let request = match serde_json::from_value::<crate::tool_runtime::SshResourceToolCall>(
            params.arguments.clone(),
        ) {
            Ok(request) if request.validate().is_ok() => request,
            _ => {
                if let Some(lc) = lifecycle.as_deref() {
                    lc.capture_payload("effective_arguments", &audit);
                }
                let result =
                    crate::ssh_resource_gateway::call(runtime, params.arguments, auth).await;
                let ok = result.get("isError").and_then(Value::as_bool) != Some(true);
                if let Some(lc) = lifecycle.as_deref() {
                    lc.dispatch_finished(true, Some(ok), if ok { "success" } else { "tool_error" });
                }
                return McpOutcome::Ok(rpc_result(
                    id,
                    if stateless_2026 {
                        mcp_stateless_result(result, false)
                    } else {
                        result
                    },
                ));
            }
        };
        let invocation = match crate::ssh_resource_gateway::invoke(
            runtime,
            request,
            recording_session_id.as_deref(),
            auth,
            crate::tool_runtime::sessions::SessionTransport::Mcp,
        )
        .await
        {
            Ok(invocation) => invocation,
            Err(SpecializedGovernanceDenial::Scope {
                required_scope,
                description,
            }) => {
                if let Some(lc) = lifecycle.as_deref() {
                    lc.capture_payload("specialized_governance", &policy.audit_projection());
                    lc.dispatch_failed("forbidden");
                    lc.dispatch_finished(false, Some(false), "forbidden");
                }
                return scope_forbidden(auth, Some(required_scope), description);
            }
            Err(SpecializedGovernanceDenial::Tool(result)) => {
                if let Some(lc) = lifecycle.as_deref() {
                    lc.capture_payload("specialized_governance", &policy.audit_projection());
                    lc.dispatch_failed("specialized_governance_denied");
                    lc.dispatch_finished(true, Some(false), "tool_error");
                }
                let result = mcp_runtime_tool_result_fallback(result);
                return McpOutcome::Ok(rpc_result(
                    id,
                    if stateless_2026 {
                        mcp_stateless_result(result, false)
                    } else {
                        result
                    },
                ));
            }
        };
        let ok = invocation.success();
        if let (Some(slot), Some(session_id)) = (
            correlation_out.as_deref_mut(),
            recording_session_id.as_deref(),
        ) {
            let mut correlation = crate::tool_runtime::ToolCallCorrelation::default();
            let project = runtime.sessions.session_project(session_id).flatten();
            correlation.resolved_project = project.clone();
            correlation.add_workflow_session(crate::tool_runtime::WorkflowSessionCorrelation {
                session_id: session_id.to_string(),
                project,
                relation: crate::tool_runtime::WorkflowSessionCorrelationRelation::Recording,
            });
            *slot = correlation;
        }
        if let Some(lc) = lifecycle.as_deref() {
            lc.capture_payload("effective_arguments", &audit);
            lc.capture_payload(
                "specialized_governance",
                &invocation.policy().audit_projection(),
            );
            lc.dispatch_finished(true, Some(ok), if ok { "success" } else { "tool_error" });
        }
        let result = invocation.to_mcp_result();
        return McpOutcome::Ok(rpc_result(
            id,
            if stateless_2026 {
                mcp_stateless_result(result, false)
            } else {
                result
            },
        ));
    }
    // Focused model surfaces reject direct tools they do not advertise at the
    // MCP boundary. Adaptive gateway calls are already reduced to an admitted
    // target and continue through the same normal runtime checks, whether that
    // target's preferred exposure is gateway or direct.
    let goal_plan_app_surface =
        server_mcp_apps_enabled && stateless_2026 && model_surface.supports_operator_extensions();
    let agent_continuation_app_surface =
        server_mcp_apps_enabled && stateless_2026 && model_surface.supports_operator_extensions();
    let app_only_goal_plan_state = goal_plan_app_surface && params.name == "goal_plan_state";
    let app_only_agent_continuation =
        agent_continuation_app_surface && is_agent_continuation_app_tool_name(&params.name);
    let surface_denied = match model_surface {
        ModelSurface::LocalCoding => !LOCAL_CODING_TOOL_NAMES.contains(&params.name.as_str()),
        ModelSurface::AdaptiveRuntime => {
            !app_only_goal_plan_state
                && !app_only_agent_continuation
                && !via_adaptive_runtime_gateway
                && !is_adaptive_runtime_direct_tool(&params.name)
        }
        ModelSurface::FullOperatorRuntime => false,
    };
    if surface_denied {
        if let Some(lc) = lifecycle.as_deref() {
            lc.dispatch_failed("surface_denied");
            lc.dispatch_finished(false, Some(false), "surface_denied");
        }
        let message = if model_surface == ModelSurface::AdaptiveRuntime {
            format!(
                "tool '{}' is not a direct adaptive_runtime tool; use the adaptive runtime gateway for discovered long-tail tools or select full_operator_runtime explicitly",
                params.name
            )
        } else {
            format!(
                "tool '{}' is not available on the local_coding MCP surface; the full operator runtime must be selected explicitly with WEBCODEX_MCP_MODEL_SURFACE=full-operator-v1",
                params.name
            )
        };
        return McpOutcome::BadRequest(rpc_error(id, -32602, message));
    }
    // From here on, the MCP boundary has established a model-visible runtime
    // tool identity. A few MCP-only validations still happen before the
    // shared ToolRuntime kernel; preserve those failed attempts in generic
    // telemetry without creating a second record for normal kernel calls.
    let mut pre_kernel_model_ergonomics = ModelErgonomicsTimer::start(&params.name);
    let resource_tool_call =
        match resources::prepare_tool_call(&params.name, stateless_2026, model_surface, auth) {
            Ok(context) => context,
            Err(error) => {
                if error.records_model_ergonomics_failure() {
                    if let (Some(slot), Some(timer)) = (
                        model_ergonomics_out.as_deref_mut(),
                        pre_kernel_model_ergonomics.take(),
                    ) {
                        *slot = Some(
                            timer
                                .finish()
                                .record_for_pre_result_failure("invalid_arguments"),
                        );
                    }
                }
                return McpOutcome::BadRequest(rpc_error(id, -32602, error.message()));
            }
        };
    let session_id = match strip_recording_session_id(&mut params.arguments) {
        Ok(session_id) => session_id,
        Err(message) => {
            if let Some(lc) = lifecycle.as_deref() {
                lc.dispatch_failed("invalid_arguments");
                lc.dispatch_finished(false, Some(false), "invalid_arguments");
            }
            if let (Some(slot), Some(timer)) = (
                model_ergonomics_out.as_deref_mut(),
                pre_kernel_model_ergonomics.take(),
            ) {
                *slot = Some(
                    timer
                        .finish()
                        .record_for_pre_result_failure("invalid_arguments"),
                );
            }
            return McpOutcome::BadRequest(rpc_error(id, -32602, message));
        }
    };
    let ack_session_message_ids = if stateless_2026 {
        match strip_stateless_ack_session_message_ids(&mut params.arguments) {
            Ok(ids) => ids,
            Err(message) => {
                if let Some(lc) = lifecycle.as_deref() {
                    lc.dispatch_failed("invalid_arguments");
                    lc.dispatch_finished(false, Some(false), "invalid_arguments");
                }
                if let (Some(slot), Some(timer)) = (
                    model_ergonomics_out.as_deref_mut(),
                    pre_kernel_model_ergonomics.take(),
                ) {
                    *slot = Some(
                        timer
                            .finish()
                            .record_for_pre_result_failure("invalid_arguments"),
                    );
                }
                return McpOutcome::BadRequest(rpc_error(id, -32602, message));
            }
        }
    } else {
        Vec::new()
    };
    let session_message_resolution = if stateless_2026 {
        match strip_stateless_session_message_resolution(&mut params.arguments) {
            Ok(value) => value,
            Err(message) => {
                if let Some(lc) = lifecycle.as_deref() {
                    lc.dispatch_failed("invalid_arguments");
                    lc.dispatch_finished(false, Some(false), "invalid_arguments");
                }
                if let (Some(slot), Some(timer)) = (
                    model_ergonomics_out.as_deref_mut(),
                    pre_kernel_model_ergonomics.take(),
                ) {
                    *slot = Some(
                        timer
                            .finish()
                            .record_for_pre_result_failure("invalid_arguments"),
                    );
                }
                return McpOutcome::BadRequest(rpc_error(id, -32602, message));
            }
        }
    } else {
        None
    };
    if session_message_resolution.is_some() && session_id.is_none() {
        let message = format!(
            "field '{}' requires '{}' for the exact target Workflow Session",
            crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD,
            crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD,
        );
        if let Some(lc) = lifecycle.as_deref() {
            lc.dispatch_failed("invalid_arguments");
            lc.dispatch_finished(false, Some(false), "invalid_arguments");
        }
        return McpOutcome::BadRequest(rpc_error(id, -32602, message));
    }
    let context_continuity_surface_capable =
        stateless_2026 && model_surface.supports_operator_extensions();
    // context_request remains surface-scoped and independent from ACK policy.
    let context_sidecar_capable = stateless_2026 && model_surface.supports_operator_extensions();
    let skill_runtime_capable = stateless_2026 && model_surface.supports_operator_extensions();
    let skill_management_capable = stateless_2026 && model_surface.supports_operator_extensions();
    let memory_surface_capable = stateless_2026 && model_surface.supports_operator_extensions();
    let trace_diagnostics_capable = stateless_2026 && model_surface.supports_operator_extensions();
    let goal_plan_app_capable = goal_plan_app_surface;
    let agent_continuation_app_capable = agent_continuation_app_surface;
    let context_request = if context_sidecar_capable {
        match strip_stateless_context_request(&mut params.arguments) {
            Ok(keys) => keys,
            Err(message) => {
                if let Some(lc) = lifecycle.as_deref() {
                    lc.dispatch_failed("invalid_arguments");
                    lc.dispatch_finished(false, Some(false), "invalid_arguments");
                }
                if let (Some(slot), Some(timer)) = (
                    model_ergonomics_out.as_deref_mut(),
                    pre_kernel_model_ergonomics.take(),
                ) {
                    *slot = Some(
                        timer
                            .finish()
                            .record_for_pre_result_failure("invalid_arguments"),
                    );
                }
                return McpOutcome::BadRequest(rpc_error(id, -32602, message));
            }
        }
    } else {
        Vec::new()
    };
    // Tolerate cached old schemas: strip the public wrapper from every operator
    // request. The adapter preserves only the normalized wire shape; whether the
    // concrete tool may consume it as continuity proof is decided by the kernel
    // from the trusted surface capability plus canonical ToolDefinition policy.
    let context_revision = context_continuity_surface_capable
        .then(|| strip_stateless_ack_session_context_revision(&mut params.arguments))
        .flatten();
    let ack_session_context_revision = if context_continuity_surface_capable {
        session_context_revision_ack_from_wire(context_revision)
    } else {
        crate::tool_runtime::sessions::SessionContextRevisionAck::Unacknowledged
    };
    if let Some(lc) = lifecycle.as_deref() {
        lc.capture_payload("effective_arguments", &params.arguments);
    }
    let as_image_requested = params.name == "read_project_artifact"
        && params.arguments.get("as_image").and_then(Value::as_bool) == Some(true);
    let outcome = runtime
        .call_tool_with_invocation_metadata(
            KernelToolCallRequest {
                tool_name: params.name.clone(),
                arguments: params.arguments,
            },
            ToolCallContext {
                transport: ToolTransport::Mcp,
                session_id: session_id.as_deref(),
                auth,
                window,
                record_oauth_scope_denials: false,
                host_file_import_trust,
            },
            ToolInvocationMetadata {
                ack_session_message_ids,
                session_message_resolution,
                context_request,
                ack_session_context_revision,
            },
            ToolProtocolCapabilities {
                context_continuity: context_continuity_surface_capable,
                context_sidecar: context_sidecar_capable,
                skill_runtime: skill_runtime_capable,
                skill_management: skill_management_capable,
                memory_surface: memory_surface_capable,
                trace_diagnostics: trace_diagnostics_capable,
                goal_plan_app: goal_plan_app_capable,
                agent_continuation_app: agent_continuation_app_capable,
            },
        )
        .await;
    let model_ergonomics_completion = outcome.model_ergonomics;
    if let Some(slot) = correlation_out.as_deref_mut() {
        *slot = outcome.correlation.clone();
    }
    let result = match outcome.error_status {
        Some(ToolCallErrorStatus::InsufficientScope {
            required_scope,
            description,
        }) => {
            if let Some(lc) = lifecycle.as_deref() {
                lc.dispatch_failed("forbidden");
                lc.dispatch_finished(false, Some(false), "forbidden");
            }
            if let (Some(slot), Some(completion)) = (
                model_ergonomics_out.as_deref_mut(),
                model_ergonomics_completion.as_ref(),
            ) {
                *slot = Some(completion.record_for_pre_result_failure("insufficient_scope"));
            }
            return scope_forbidden(auth, required_scope, description);
        }
        Some(ToolCallErrorStatus::InvalidArguments { message }) => {
            if let Some(lc) = lifecycle.as_deref() {
                lc.dispatch_failed("invalid_arguments");
                lc.dispatch_finished(false, Some(false), "invalid_arguments");
            }
            if let (Some(slot), Some(completion)) = (
                model_ergonomics_out.as_deref_mut(),
                model_ergonomics_completion.as_ref(),
            ) {
                *slot = Some(completion.record_for_pre_result_failure("invalid_arguments"));
            }
            return McpOutcome::BadRequest(rpc_error(id, -32602, message));
        }
        None => outcome
            .result
            .expect("tool kernel outcome without error must include result"),
    };
    debug_assert_eq!(outcome.success, result.success);
    if let Some(lc) = lifecycle.as_deref() {
        // Protocol layer produced a JSON-RPC result (not -32xxx).
        // Tool kernel success is independent (isError / structuredContent).
        let category = if result.success {
            "success"
        } else {
            "tool_error"
        };
        if result.success {
            lc.dispatch_finished(true, Some(true), category);
        } else {
            lc.dispatch_finished(true, Some(false), category);
        }
    }
    let mut result = match resources::adapt_tool_result(
        &params.name,
        as_image_requested,
        result,
        resource_tool_call,
    ) {
        resources::McpResourceToolResultAdaptation::Framed(value) => value,
        resources::McpResourceToolResultAdaptation::Unhandled(result) => {
            // App-only tools use the standard CallToolResult channel too. Their
            // visibility/admission boundary, not custom result metadata, keeps
            // continuation protocol data out of ordinary model tool results.
            mcp_runtime_tool_result_fallback(result)
        }
    };
    if app_only_agent_continuation {
        // ChatGPT production has been observed to complete View-originated
        // tools/call server-side while not forwarding structuredContent back to
        // the View. Keep structuredContent canonical, but duplicate this bounded
        // app-only envelope into standard text content as a compatibility path.
        // These tools are ModelHidden/app-visible only, so ordinary model tool
        // results retain the compact text fallback.
        attach_app_tool_content_fallback(&mut result);
    }
    if app_enabled {
        presentation::attach_result_app_presentation(&params.name, &mut result);
    }
    let model_ergonomics = model_ergonomics_completion.as_ref().and_then(|completion| {
        result
            .get("structuredContent")
            .and_then(|structured| completion.record_for_structured_content(structured))
    });
    if let Some(slot) = model_ergonomics_out.as_deref_mut() {
        *slot = model_ergonomics;
    }
    return McpOutcome::Ok(rpc_result(
        id,
        if stateless_2026 {
            mcp_stateless_result(result, false)
        } else {
            result
        },
    ));
}
