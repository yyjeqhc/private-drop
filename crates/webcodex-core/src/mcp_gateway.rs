//! Bounded, transport-neutral protocol for the Runner-owned stdio MCP gateway.
//!
//! This is intentionally not a raw JSON-RPC tunnel. Provider inventory rides
//! normal Runner registration; request traffic contains only passive provider
//! lifecycle status, `tools/list`, and `tools/call`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

pub const MCP_GATEWAY_MAX_PROVIDERS: usize = 8;
pub const MCP_GATEWAY_MAX_PROVIDER_ID_BYTES: usize = 64;
pub const MCP_GATEWAY_MAX_PROVIDER_NAME_BYTES: usize = 128;
pub const MCP_GATEWAY_MAX_TOOL_COUNT: usize = 128;
pub const MCP_GATEWAY_MAX_TOOL_NAME_BYTES: usize = 128;
pub const MCP_GATEWAY_MAX_DESCRIPTION_BYTES: usize = 4 * 1024;
pub const MCP_GATEWAY_MAX_SCHEMA_BYTES: usize = 64 * 1024;
pub const MCP_GATEWAY_MAX_ARGUMENT_BYTES: usize = 64 * 1024;
pub const MCP_GATEWAY_MAX_STRUCTURED_CONTENT_BYTES: usize = 512 * 1024;
pub const MCP_GATEWAY_MAX_TEXT_CONTENT_BYTES: usize = 512 * 1024;
pub const MCP_GATEWAY_MAX_RESULT_BYTES: usize = 512 * 1024;
pub const MCP_GATEWAY_MAX_CONTENT_ITEMS: usize = 32;
pub const MCP_GATEWAY_MAX_MESSAGE_BYTES: usize = 1024 * 1024;
pub const MCP_GATEWAY_MAX_JSON_DEPTH: usize = 16;
pub const MCP_GATEWAY_MAX_JSON_NODES: usize = 4_096;
pub const MCP_GATEWAY_MAX_JSON_STRING_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpGatewayRequest {
    ProviderStatus {
        provider_id: String,
        provider_instance_id: String,
    },
    ToolsList {
        provider_id: String,
        provider_instance_id: String,
    },
    ToolsCall {
        provider_id: String,
        provider_instance_id: String,
        name: String,
        arguments: Value,
        expected_schema: McpGatewaySchemaObservation,
    },
}

impl McpGatewayRequest {
    pub fn provider_id(&self) -> &str {
        match self {
            Self::ProviderStatus { provider_id, .. }
            | Self::ToolsList { provider_id, .. }
            | Self::ToolsCall { provider_id, .. } => provider_id,
        }
    }

    pub fn provider_instance_id(&self) -> &str {
        match self {
            Self::ProviderStatus {
                provider_instance_id,
                ..
            }
            | Self::ToolsList {
                provider_instance_id,
                ..
            }
            | Self::ToolsCall {
                provider_instance_id,
                ..
            } => provider_instance_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpGatewayDispatchState {
    NotStarted,
    OutcomeUnknown,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpGatewayProviderState {
    NeverStarted,
    Healthy,
    ConnectionRetired,
    Busy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpGatewayProvider {
    pub provider_id: String,
    pub provider_instance_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpGatewayTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(
        default,
        rename = "outputSchema",
        skip_serializing_if = "Option::is_none"
    )]
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpGatewaySchemaObservation {
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(
        default,
        rename = "outputSchema",
        skip_serializing_if = "Option::is_none"
    )]
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
}

impl McpGatewayTool {
    pub fn schema_observation(&self) -> McpGatewaySchemaObservation {
        McpGatewaySchemaObservation {
            input_schema: self.input_schema.clone(),
            output_schema: self.output_schema.clone(),
            annotations: self.annotations.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum McpGatewayContent {
    Text { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpGatewayToolResult {
    pub content: Vec<McpGatewayContent>,
    #[serde(
        default,
        rename = "structuredContent",
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_content: Option<Value>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpGatewayResponsePayload {
    ProviderStatus { state: McpGatewayProviderState },
    Tools { tools: Vec<McpGatewayTool> },
    ToolResult { result: McpGatewayToolResult },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpGatewayError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpGatewayResponse {
    pub dispatch_state: McpGatewayDispatchState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<McpGatewayResponsePayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<McpGatewayError>,
}

impl McpGatewayResponse {
    pub fn success(payload: McpGatewayResponsePayload) -> Self {
        Self {
            dispatch_state: McpGatewayDispatchState::Completed,
            payload: Some(payload),
            error: None,
        }
    }

    pub fn error(
        dispatch_state: McpGatewayDispatchState,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            dispatch_state,
            payload: None,
            error: Some(McpGatewayError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

pub fn validate_provider_id(value: &str) -> Result<(), String> {
    validate_identifier(
        value,
        "provider_id",
        MCP_GATEWAY_MAX_PROVIDER_ID_BYTES,
        true,
    )
}

pub fn validate_provider_instance_id(value: &str) -> Result<(), String> {
    validate_identifier(
        value,
        "provider_instance_id",
        MCP_GATEWAY_MAX_PROVIDER_ID_BYTES,
        false,
    )
}

pub fn validate_tool_name(value: &str) -> Result<(), String> {
    validate_identifier(value, "tool name", MCP_GATEWAY_MAX_TOOL_NAME_BYTES, false)
}

fn validate_identifier(
    value: &str,
    field: &str,
    max_bytes: usize,
    lowercase_only: bool,
) -> Result<(), String> {
    if value.is_empty() || value.len() > max_bytes {
        return Err(format!("{field} must contain 1..={max_bytes} bytes"));
    }
    let valid = value.bytes().all(|byte| {
        (if lowercase_only {
            byte.is_ascii_lowercase()
        } else {
            byte.is_ascii_alphanumeric()
        }) || byte.is_ascii_digit()
            || matches!(byte, b'_' | b'-' | b'.')
    });
    if !valid {
        let letters = if lowercase_only {
            "lowercase ASCII letters"
        } else {
            "ASCII letters"
        };
        return Err(format!(
            "{field} may contain only {letters}, digits, '_', '-', and '.'"
        ));
    }
    Ok(())
}

pub fn validate_provider_name(value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > MCP_GATEWAY_MAX_PROVIDER_NAME_BYTES {
        return Err(format!(
            "provider name must contain 1..={} bytes",
            MCP_GATEWAY_MAX_PROVIDER_NAME_BYTES
        ));
    }
    if value.chars().any(char::is_control) {
        return Err("provider name contains unsupported control characters".to_string());
    }
    Ok(())
}

pub fn validate_request(request: &McpGatewayRequest) -> Result<(), String> {
    match request {
        McpGatewayRequest::ProviderStatus {
            provider_id,
            provider_instance_id,
        }
        | McpGatewayRequest::ToolsList {
            provider_id,
            provider_instance_id,
        } => {
            validate_provider_id(provider_id)?;
            validate_provider_instance_id(provider_instance_id)
        }
        McpGatewayRequest::ToolsCall {
            provider_id,
            provider_instance_id,
            name,
            arguments,
            expected_schema,
        } => {
            validate_provider_id(provider_id)?;
            validate_provider_instance_id(provider_instance_id)?;
            validate_tool_name(name)?;
            if !arguments.is_object() {
                return Err("tool arguments must be a JSON object".to_string());
            }
            validate_json_value(arguments, MCP_GATEWAY_MAX_ARGUMENT_BYTES, "tool arguments")?;
            validate_schema_observation(expected_schema)?;
            Ok(())
        }
    }
}

pub fn validate_response(response: &McpGatewayResponse) -> Result<(), String> {
    let encoded = serde_json::to_vec(response)
        .map_err(|_| "bridge response could not be serialized".to_string())?;
    if encoded.len() > MCP_GATEWAY_MAX_MESSAGE_BYTES {
        return Err(format!(
            "bridge response exceeds maximum {} bytes",
            MCP_GATEWAY_MAX_MESSAGE_BYTES
        ));
    }
    match (&response.payload, &response.error) {
        (Some(_), Some(_)) | (None, None) => {
            return Err("bridge response must contain exactly one payload or error".to_string())
        }
        (Some(_), None) if response.dispatch_state != McpGatewayDispatchState::Completed => {
            return Err("bridge response payload requires completed dispatch state".to_string())
        }
        (None, Some(error)) => validate_error(error)?,
        _ => {}
    }
    match response.payload.as_ref() {
        Some(McpGatewayResponsePayload::ProviderStatus { .. }) => Ok(()),
        Some(McpGatewayResponsePayload::Tools { tools }) => validate_tools(tools),
        Some(McpGatewayResponsePayload::ToolResult { result }) => validate_tool_result(result),
        None => Ok(()),
    }
}

fn validate_error(error: &McpGatewayError) -> Result<(), String> {
    validate_identifier(&error.code, "bridge error code", 80, true)?;
    if error.message.is_empty() || error.message.len() > 512 {
        return Err("bridge error message must contain 1..=512 bytes".to_string());
    }
    validate_text_controls(&error.message, "bridge error message")
}

pub fn validate_providers(providers: &[McpGatewayProvider]) -> Result<(), String> {
    if providers.len() > MCP_GATEWAY_MAX_PROVIDERS {
        return Err(format!(
            "provider count exceeds maximum {}",
            MCP_GATEWAY_MAX_PROVIDERS
        ));
    }
    let mut provider_ids = HashSet::new();
    let mut instance_ids = HashSet::new();
    for provider in providers {
        validate_provider_id(&provider.provider_id)?;
        validate_provider_instance_id(&provider.provider_instance_id)?;
        validate_provider_name(&provider.name)?;
        if !provider_ids.insert(provider.provider_id.as_str()) {
            return Err("duplicate bridge provider id".to_string());
        }
        if !instance_ids.insert(provider.provider_instance_id.as_str()) {
            return Err("duplicate bridge provider instance identity".to_string());
        }
    }
    Ok(())
}

pub fn validate_tools(tools: &[McpGatewayTool]) -> Result<(), String> {
    if tools.len() > MCP_GATEWAY_MAX_TOOL_COUNT {
        return Err(format!(
            "tool count exceeds maximum {}",
            MCP_GATEWAY_MAX_TOOL_COUNT
        ));
    }
    let mut names = HashSet::new();
    for tool in tools {
        validate_tool_name(&tool.name)?;
        if !names.insert(tool.name.as_str()) {
            return Err(format!("duplicate tool name '{}'", tool.name));
        }
        if let Some(title) = tool.title.as_deref() {
            if title.is_empty() || title.len() > MCP_GATEWAY_MAX_PROVIDER_NAME_BYTES {
                return Err("tool title is empty or too long".to_string());
            }
            validate_text_controls(title, "tool title")?;
        }
        if let Some(description) = tool.description.as_deref() {
            if description.len() > MCP_GATEWAY_MAX_DESCRIPTION_BYTES {
                return Err(format!(
                    "tool description exceeds maximum {} bytes",
                    MCP_GATEWAY_MAX_DESCRIPTION_BYTES
                ));
            }
            validate_text_controls(description, "tool description")?;
        }
        validate_schema_observation(&tool.schema_observation())?;
        if let Some(meta) = tool.meta.as_ref() {
            if !meta.is_object() {
                return Err("tool _meta must be a JSON object".to_string());
            }
            validate_json_value(meta, MCP_GATEWAY_MAX_SCHEMA_BYTES, "tool _meta")?;
        }
    }
    Ok(())
}

pub fn validate_schema_observation(
    observation: &McpGatewaySchemaObservation,
) -> Result<(), String> {
    if !observation.input_schema.is_object() {
        return Err("tool inputSchema must be a JSON object".to_string());
    }
    validate_json_value(
        &observation.input_schema,
        MCP_GATEWAY_MAX_SCHEMA_BYTES,
        "tool inputSchema",
    )?;
    if let Some(output_schema) = observation.output_schema.as_ref() {
        if !output_schema.is_object() {
            return Err("tool outputSchema must be a JSON object".to_string());
        }
        validate_json_value(
            output_schema,
            MCP_GATEWAY_MAX_SCHEMA_BYTES,
            "tool outputSchema",
        )?;
    }
    if let Some(annotations) = observation.annotations.as_ref() {
        if !annotations.is_object() {
            return Err("tool annotations must be a JSON object".to_string());
        }
        validate_json_value(
            annotations,
            MCP_GATEWAY_MAX_SCHEMA_BYTES,
            "tool annotations",
        )?;
    }
    Ok(())
}

pub fn validate_tool_result(result: &McpGatewayToolResult) -> Result<(), String> {
    if result.content.len() > MCP_GATEWAY_MAX_CONTENT_ITEMS {
        return Err(format!(
            "tool result content exceeds maximum {} items",
            MCP_GATEWAY_MAX_CONTENT_ITEMS
        ));
    }
    let mut text_bytes = 0usize;
    for content in &result.content {
        match content {
            McpGatewayContent::Text { text } => {
                if text.len() > MCP_GATEWAY_MAX_TEXT_CONTENT_BYTES {
                    return Err(format!(
                        "tool result text exceeds maximum {} bytes",
                        MCP_GATEWAY_MAX_TEXT_CONTENT_BYTES
                    ));
                }
                validate_text_controls(text, "tool result text")?;
                text_bytes = text_bytes.saturating_add(text.len());
            }
        }
    }
    if let Some(structured) = result.structured_content.as_ref() {
        if !structured.is_object() {
            return Err("structured tool result must be a JSON object".to_string());
        }
        validate_json_value(
            structured,
            MCP_GATEWAY_MAX_STRUCTURED_CONTENT_BYTES,
            "structured tool result",
        )?;
    }
    let encoded = serde_json::to_vec(result)
        .map_err(|_| "tool result could not be serialized".to_string())?;
    if encoded.len() > MCP_GATEWAY_MAX_RESULT_BYTES || text_bytes > MCP_GATEWAY_MAX_RESULT_BYTES {
        return Err(format!(
            "tool result exceeds maximum {} bytes",
            MCP_GATEWAY_MAX_RESULT_BYTES
        ));
    }
    Ok(())
}

pub fn validate_json_value(value: &Value, max_bytes: usize, field: &str) -> Result<(), String> {
    let encoded =
        serde_json::to_vec(value).map_err(|_| format!("{field} could not be serialized"))?;
    if encoded.len() > max_bytes {
        return Err(format!("{field} exceeds maximum {max_bytes} bytes"));
    }
    let mut nodes = 0usize;
    validate_json_node(value, 0, &mut nodes, field)
}

fn validate_json_node(
    value: &Value,
    depth: usize,
    nodes: &mut usize,
    field: &str,
) -> Result<(), String> {
    if depth > MCP_GATEWAY_MAX_JSON_DEPTH {
        return Err(format!(
            "{field} exceeds maximum JSON depth {}",
            MCP_GATEWAY_MAX_JSON_DEPTH
        ));
    }
    *nodes = nodes.saturating_add(1);
    if *nodes > MCP_GATEWAY_MAX_JSON_NODES {
        return Err(format!(
            "{field} exceeds maximum JSON node count {}",
            MCP_GATEWAY_MAX_JSON_NODES
        ));
    }
    match value {
        Value::String(text) => {
            if text.len() > MCP_GATEWAY_MAX_JSON_STRING_BYTES {
                return Err(format!(
                    "{field} contains a string larger than {} bytes",
                    MCP_GATEWAY_MAX_JSON_STRING_BYTES
                ));
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_json_node(value, depth + 1, nodes, field)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                if key.len() > 1_024 || key.chars().any(char::is_control) {
                    return Err(format!("{field} contains an invalid object key"));
                }
                validate_json_node(value, depth + 1, nodes, field)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn validate_text_controls(value: &str, field: &str) -> Result<(), String> {
    if value
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(format!("{field} contains unsupported control characters"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_recursive_and_excessive_values() {
        let mut value = json!({});
        for _ in 0..=MCP_GATEWAY_MAX_JSON_DEPTH {
            value = json!({"next": value});
        }
        assert!(
            validate_json_value(&value, MCP_GATEWAY_MAX_SCHEMA_BYTES, "schema")
                .unwrap_err()
                .contains("depth")
        );

        let arguments = json!({"value": "x".repeat(MCP_GATEWAY_MAX_ARGUMENT_BYTES)});
        assert!(
            validate_json_value(&arguments, MCP_GATEWAY_MAX_ARGUMENT_BYTES, "arguments").is_err()
        );
    }

    #[test]
    fn tool_result_bounds_allow_512_kib_outputs_without_expanding_inputs_or_messages() {
        assert_eq!(MCP_GATEWAY_MAX_ARGUMENT_BYTES, 64 * 1024);
        assert_eq!(MCP_GATEWAY_MAX_SCHEMA_BYTES, 64 * 1024);
        assert_eq!(MCP_GATEWAY_MAX_TEXT_CONTENT_BYTES, 512 * 1024);
        assert_eq!(MCP_GATEWAY_MAX_STRUCTURED_CONTENT_BYTES, 512 * 1024);
        assert_eq!(MCP_GATEWAY_MAX_RESULT_BYTES, 512 * 1024);
        assert_eq!(MCP_GATEWAY_MAX_JSON_STRING_BYTES, 512 * 1024);
        assert_eq!(MCP_GATEWAY_MAX_MESSAGE_BYTES, 1024 * 1024);

        let large_text = McpGatewayToolResult {
            content: vec![McpGatewayContent::Text {
                text: "x".repeat(384 * 1024),
            }],
            structured_content: None,
            is_error: false,
        };
        validate_tool_result(&large_text).unwrap();

        let large_structured = McpGatewayToolResult {
            content: vec![],
            structured_content: Some(json!({"payload": "x".repeat(384 * 1024)})),
            is_error: false,
        };
        validate_tool_result(&large_structured).unwrap();

        let aggregate_oversized = McpGatewayToolResult {
            content: vec![McpGatewayContent::Text {
                text: "x".repeat(300 * 1024),
            }],
            structured_content: Some(json!({"payload": "y".repeat(300 * 1024)})),
            is_error: false,
        };
        assert!(validate_tool_result(&aggregate_oversized).is_err());

        let oversized_text = McpGatewayToolResult {
            content: vec![McpGatewayContent::Text {
                text: "x".repeat(MCP_GATEWAY_MAX_TEXT_CONTENT_BYTES + 1),
            }],
            structured_content: None,
            is_error: false,
        };
        assert!(validate_tool_result(&oversized_text).is_err());

        let oversized_structured = McpGatewayToolResult {
            content: vec![],
            structured_content: Some(json!({
                "payload": "x".repeat(MCP_GATEWAY_MAX_STRUCTURED_CONTENT_BYTES + 1)
            })),
            is_error: false,
        };
        assert!(validate_tool_result(&oversized_structured).is_err());

        let oversized_arguments = json!({"value": "x".repeat(MCP_GATEWAY_MAX_ARGUMENT_BYTES)});
        assert!(validate_json_value(
            &oversized_arguments,
            MCP_GATEWAY_MAX_ARGUMENT_BYTES,
            "arguments"
        )
        .is_err());
        let oversized_schema = json!({"description": "x".repeat(MCP_GATEWAY_MAX_SCHEMA_BYTES)});
        assert!(
            validate_json_value(&oversized_schema, MCP_GATEWAY_MAX_SCHEMA_BYTES, "schema").is_err()
        );
    }

    #[test]
    fn provider_status_wire_is_bounded_and_contains_no_local_process_details() {
        let request = McpGatewayRequest::ProviderStatus {
            provider_id: "provider".to_string(),
            provider_instance_id: "instance".to_string(),
        };
        validate_request(&request).unwrap();
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["operation"], "provider_status");
        assert!(encoded.get("pid").is_none());
        assert!(encoded.get("executable").is_none());

        let response = McpGatewayResponse::success(McpGatewayResponsePayload::ProviderStatus {
            state: McpGatewayProviderState::NeverStarted,
        });
        validate_response(&response).unwrap();
        let encoded = serde_json::to_value(response).unwrap();
        assert_eq!(encoded["payload"]["state"], "never_started");
    }

    #[test]
    fn tools_call_wire_has_no_caller_meta_but_tool_descriptor_meta_is_preserved() {
        let request = McpGatewayRequest::ToolsCall {
            provider_id: "provider".to_string(),
            provider_instance_id: "instance".to_string(),
            name: "echo".to_string(),
            arguments: json!({"value": "hello"}),
            expected_schema: McpGatewaySchemaObservation {
                input_schema: json!({"type": "object"}),
                output_schema: None,
                annotations: None,
            },
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert!(encoded.get("_meta").is_none());
        let mut injected = encoded;
        injected["_meta"] = json!({"progressToken": "outer"});
        assert!(serde_json::from_value::<McpGatewayRequest>(injected).is_err());

        let tool = McpGatewayTool {
            name: "echo".to_string(),
            title: None,
            description: None,
            input_schema: json!({"type": "object"}),
            output_schema: None,
            annotations: None,
            meta: Some(json!({"providerExtension": true})),
        };
        validate_tools(std::slice::from_ref(&tool)).unwrap();
        assert_eq!(
            serde_json::to_value(tool).unwrap()["_meta"]["providerExtension"],
            true
        );
    }

    #[test]
    fn rejects_duplicate_tools_and_binary_content() {
        let tool = McpGatewayTool {
            name: "echo".to_string(),
            title: None,
            description: None,
            input_schema: json!({"type": "object"}),
            output_schema: None,
            annotations: None,
            meta: None,
        };
        assert!(validate_tools(&[tool.clone(), tool]).is_err());

        let raw = json!({
            "content": [{"type": "image", "data": "AA==", "mimeType": "image/png"}],
            "isError": false
        });
        assert!(serde_json::from_value::<McpGatewayToolResult>(raw).is_err());
    }
}
