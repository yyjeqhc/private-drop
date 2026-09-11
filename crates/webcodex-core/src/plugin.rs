//! Bounded transport contracts for Runner-owned native Tool Plugins.
//!
//! Plugins speak the WebCodex Plugin Protocol over local stdio.  This module is
//! deliberately independent from MCP: concrete provider tools stay behind the
//! exact Runner gateway, while these types describe the native Runner protocol
//! and the closed Server <-> Runner gateway.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

pub const PLUGIN_PROTOCOL_VERSION: &str = "webcodex-plugin-v1";
pub const PLUGIN_MAX_PROVIDERS: usize = 8;
pub const PLUGIN_MAX_PROVIDER_ID_BYTES: usize = 64;
pub const PLUGIN_MAX_PROVIDER_NAME_BYTES: usize = 128;
pub const PLUGIN_MAX_TOOL_COUNT: usize = 128;
pub const PLUGIN_MAX_TOOL_NAME_BYTES: usize = 128;
pub const PLUGIN_MAX_DESCRIPTION_BYTES: usize = 4 * 1024;
pub const PLUGIN_MAX_SCHEMA_BYTES: usize = 64 * 1024;
pub const PLUGIN_MAX_ARGUMENT_BYTES: usize = 64 * 1024;
pub const PLUGIN_MAX_STRUCTURED_CONTENT_BYTES: usize = 512 * 1024;
pub const PLUGIN_MAX_TEXT_CONTENT_BYTES: usize = 512 * 1024;
pub const PLUGIN_MAX_RESULT_BYTES: usize = 512 * 1024;
pub const PLUGIN_MAX_CONTENT_ITEMS: usize = 32;
pub const PLUGIN_MAX_MESSAGE_BYTES: usize = 1024 * 1024;
pub const PLUGIN_MAX_JSON_DEPTH: usize = 16;
pub const PLUGIN_MAX_JSON_NODES: usize = 4_096;
pub const PLUGIN_MAX_JSON_STRING_BYTES: usize = 512 * 1024;
pub const PLUGIN_MAX_CHECK_DETAIL_BYTES: usize = 512;
pub const PLUGIN_PROJECT_CATALOG_REVISION_PREFIX: &str = "wc_plugcat_";
pub const PLUGIN_MAX_PROJECT_CATALOG_DESCRIPTION_BYTES: usize = 512;
pub const PLUGIN_MAX_PROJECT_CATALOG_ENTRIES: usize = PLUGIN_MAX_PROVIDERS * PLUGIN_MAX_TOOL_COUNT;
pub const PLUGIN_SCHEMA_MAX_PROPERTIES: usize = 128;
pub const PLUGIN_SCHEMA_MAX_REQUIRED: usize = 128;
pub const PLUGIN_SCHEMA_MAX_ENUM_VALUES: usize = 128;
pub const PLUGIN_CATALOG_DIGEST_PREFIX: &str = "sha256:";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginGatewayRequest {
    Check {
        provider_id: String,
    },
    Reload,
    ProjectCatalog {
        project_id: String,
    },
    ProvidersList,
    ToolsList {
        provider_id: String,
        provider_instance_id: String,
    },
    ToolsCall {
        provider_id: String,
        provider_instance_id: String,
        name: String,
        arguments: Value,
        expected_schema: PluginSchemaObservation,
    },
}

impl PluginGatewayRequest {
    pub fn provider_binding(&self) -> Option<(&str, &str)> {
        match self {
            Self::ToolsList {
                provider_id,
                provider_instance_id,
            }
            | Self::ToolsCall {
                provider_id,
                provider_instance_id,
                ..
            } => Some((provider_id, provider_instance_id)),
            Self::Check { .. }
            | Self::Reload
            | Self::ProjectCatalog { .. }
            | Self::ProvidersList => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginDispatchState {
    NotStarted,
    OutcomeUnknown,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginTool {
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSchemaObservation {
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

impl PluginTool {
    pub fn schema_observation(&self) -> PluginSchemaObservation {
        PluginSchemaObservation {
            input_schema: self.input_schema.clone(),
            output_schema: self.output_schema.clone(),
            annotations: self.annotations.clone(),
        }
    }
}

/// Canonical, validated catalog admitted from one exact Plugin provider process.
/// Tool order is normalized by name and the digest is computed from a recursively
/// key-sorted JSON projection, so map iteration order cannot change identity.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginCatalog {
    tools: Vec<PluginTool>,
    digest: String,
}

impl PluginCatalog {
    pub fn admit(mut tools: Vec<PluginTool>) -> Result<Self, String> {
        validate_tools(&tools)?;
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        let digest = plugin_catalog_digest_from_canonical_tools(&tools)?;
        Ok(Self { tools, digest })
    }

    pub fn tools(&self) -> &[PluginTool] {
        &self.tools
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn tool(&self, name: &str) -> Option<&PluginTool> {
        self.tools.iter().find(|tool| tool.name == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum PluginContent {
    Text { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginToolResult {
    pub content: Vec<PluginContent>,
    #[serde(
        default,
        rename = "structuredContent",
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_content: Option<Value>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginProviderView {
    pub provider_id: String,
    pub provider_instance_id: String,
    pub name: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSelectionAnnotations {
    #[serde(
        default,
        rename = "readOnlyHint",
        skip_serializing_if = "Option::is_none"
    )]
    pub read_only_hint: Option<bool>,
    #[serde(
        default,
        rename = "destructiveHint",
        skip_serializing_if = "Option::is_none"
    )]
    pub destructive_hint: Option<bool>,
    #[serde(
        default,
        rename = "idempotentHint",
        skip_serializing_if = "Option::is_none"
    )]
    pub idempotent_hint: Option<bool>,
    #[serde(
        default,
        rename = "openWorldHint",
        skip_serializing_if = "Option::is_none"
    )]
    pub open_world_hint: Option<bool>,
}

impl PluginSelectionAnnotations {
    pub fn from_value(value: Option<&Value>) -> Self {
        let boolean = |key| {
            value
                .and_then(Value::as_object)
                .and_then(|object| object.get(key))
                .and_then(Value::as_bool)
        };
        Self {
            read_only_hint: boolean("readOnlyHint"),
            destructive_hint: boolean("destructiveHint"),
            idempotent_hint: boolean("idempotentHint"),
            open_world_hint: boolean("openWorldHint"),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.read_only_hint.is_none()
            && self.destructive_hint.is_none()
            && self.idempotent_hint.is_none()
            && self.open_world_hint.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPluginCatalogEntry {
    pub plugin: String,
    pub name: String,
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "PluginSelectionAnnotations::is_empty")]
    pub annotations: PluginSelectionAnnotations,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPluginCatalog {
    pub catalog_revision: String,
    pub total_count: usize,
    pub entries: Vec<ProjectPluginCatalogEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginReloadFailure {
    pub provider_id: String,
    pub code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginCheckPhase {
    Config,
    Environment,
    Executable,
    Spawn,
    Stdio,
    Initialize,
    ToolsList,
    Validation,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCheckToolSummary {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCheckDiagnostic {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCheckReport {
    pub provider_id: String,
    pub ready: bool,
    pub phase: PluginCheckPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub tool_count: usize,
    #[serde(default)]
    pub tools: Vec<PluginCheckToolSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<PluginCheckDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginGatewayResponsePayload {
    Checked {
        report: PluginCheckReport,
    },
    Providers {
        providers: Vec<PluginProviderView>,
    },
    ProjectCatalog {
        catalog: ProjectPluginCatalog,
    },
    Reloaded {
        providers: Vec<PluginProviderView>,
        failures: Vec<PluginReloadFailure>,
    },
    Tools {
        tools: Vec<PluginTool>,
    },
    ToolResult {
        result: PluginToolResult,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginGatewayError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginGatewayResponse {
    pub dispatch_state: PluginDispatchState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<PluginGatewayResponsePayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<PluginGatewayError>,
}

impl PluginGatewayResponse {
    pub fn success(payload: PluginGatewayResponsePayload) -> Self {
        Self {
            dispatch_state: PluginDispatchState::Completed,
            payload: Some(payload),
            error: None,
        }
    }

    pub fn error(
        dispatch_state: PluginDispatchState,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            dispatch_state,
            payload: None,
            error: Some(PluginGatewayError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

pub fn validate_provider_id(value: &str) -> Result<(), String> {
    validate_identifier(value, "provider_id", PLUGIN_MAX_PROVIDER_ID_BYTES, true)
}

pub fn validate_provider_instance_id(value: &str) -> Result<(), String> {
    validate_identifier(
        value,
        "provider_instance_id",
        PLUGIN_MAX_PROVIDER_ID_BYTES,
        false,
    )
}

pub fn validate_tool_name(value: &str) -> Result<(), String> {
    validate_identifier(value, "tool name", PLUGIN_MAX_TOOL_NAME_BYTES, false)
}

pub fn validate_provider_name(value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > PLUGIN_MAX_PROVIDER_NAME_BYTES {
        return Err(format!(
            "provider name must contain 1..={} bytes",
            PLUGIN_MAX_PROVIDER_NAME_BYTES
        ));
    }
    validate_text_controls(value, "provider name")
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
        return Err(format!("{field} contains unsupported characters"));
    }
    Ok(())
}

pub fn validate_request(request: &PluginGatewayRequest) -> Result<(), String> {
    match request {
        PluginGatewayRequest::Check { provider_id } => validate_provider_id(provider_id),
        PluginGatewayRequest::Reload | PluginGatewayRequest::ProvidersList => Ok(()),
        PluginGatewayRequest::ProjectCatalog { project_id } => {
            if project_id.trim().is_empty()
                || project_id.len() > 512
                || project_id.chars().any(char::is_control)
            {
                Err("project_id is invalid for Plugin project catalog discovery".to_string())
            } else {
                Ok(())
            }
        }
        PluginGatewayRequest::ToolsList {
            provider_id,
            provider_instance_id,
            ..
        } => {
            validate_provider_id(provider_id)?;
            validate_provider_instance_id(provider_instance_id)
        }
        PluginGatewayRequest::ToolsCall {
            provider_id,
            provider_instance_id,
            name,
            arguments,
            expected_schema,
            ..
        } => {
            validate_provider_id(provider_id)?;
            validate_provider_instance_id(provider_instance_id)?;
            validate_tool_name(name)?;
            if !arguments.is_object() {
                return Err("tool arguments must be a JSON object".to_string());
            }
            validate_json_value(arguments, PLUGIN_MAX_ARGUMENT_BYTES, "tool arguments")?;
            validate_schema_observation(expected_schema)
        }
    }
}

fn plugin_catalog_digest_from_canonical_tools(tools: &[PluginTool]) -> Result<String, String> {
    let value = serde_json::to_value(tools)
        .map_err(|_| "plugin catalog could not be serialized".to_string())?;
    let mut canonical = Vec::new();
    write_canonical_json(&value, &mut canonical)?;
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex.plugin-catalog.v1\0");
    hasher.update(&canonical);
    Ok(format!(
        "{PLUGIN_CATALOG_DIGEST_PREFIX}{:x}",
        hasher.finalize()
    ))
}

fn write_canonical_json(value: &Value, output: &mut Vec<u8>) -> Result<(), String> {
    match value {
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical_json(value, output)?;
            }
            output.push(b']');
        }
        Value::Object(values) => {
            output.push(b'{');
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                output.extend(
                    serde_json::to_vec(key)
                        .map_err(|_| "plugin catalog key could not be serialized".to_string())?,
                );
                output.push(b':');
                write_canonical_json(&values[key], output)?;
            }
            output.push(b'}');
        }
        _ => output.extend(
            serde_json::to_vec(value)
                .map_err(|_| "plugin catalog value could not be serialized".to_string())?,
        ),
    }
    Ok(())
}

pub fn validate_plugin_catalog_digest(value: &str) -> Result<(), String> {
    let Some(hex) = value.strip_prefix(PLUGIN_CATALOG_DIGEST_PREFIX) else {
        return Err("plugin catalog digest must use sha256 namespace".to_string());
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("plugin catalog digest must contain 64 lowercase hex digits".to_string());
    }
    Ok(())
}

pub fn validate_project_plugin_catalog_revision(value: &str) -> Result<(), String> {
    let Some(hex) = value.strip_prefix(PLUGIN_PROJECT_CATALOG_REVISION_PREFIX) else {
        return Err("project Plugin catalog revision has invalid namespace".to_string());
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(
            "project Plugin catalog revision must contain 64 lowercase hex digits".to_string(),
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaProfileFailureKind {
    Invalid,
    UnsupportedKeyword,
}

#[derive(Debug, Clone, Copy)]
struct SchemaProfileFailure {
    kind: SchemaProfileFailureKind,
    message: &'static str,
}

impl SchemaProfileFailure {
    fn invalid(message: &'static str) -> Self {
        Self {
            kind: SchemaProfileFailureKind::Invalid,
            message,
        }
    }

    fn unsupported() -> Self {
        Self {
            kind: SchemaProfileFailureKind::UnsupportedKeyword,
            message: "schema contains a keyword outside the Native Plugin Schema Profile",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginSchemaType {
    Object,
    Array,
    String,
    Number,
    Integer,
    Boolean,
    Null,
}

fn parse_plugin_schema_type(value: &str) -> Option<PluginSchemaType> {
    match value {
        "object" => Some(PluginSchemaType::Object),
        "array" => Some(PluginSchemaType::Array),
        "string" => Some(PluginSchemaType::String),
        "number" => Some(PluginSchemaType::Number),
        "integer" => Some(PluginSchemaType::Integer),
        "boolean" => Some(PluginSchemaType::Boolean),
        "null" => Some(PluginSchemaType::Null),
        _ => None,
    }
}

fn preflight_plugin_schema_profile(
    schema: &Value,
    require_object_root: bool,
    max_string_schema_length: usize,
) -> Result<(), SchemaProfileFailure> {
    let schema_type = preflight_plugin_schema_node(schema, 0, max_string_schema_length)?;
    if require_object_root && schema_type != PluginSchemaType::Object {
        return Err(SchemaProfileFailure::invalid(
            "tool schema root type must be object",
        ));
    }
    Ok(())
}

fn preflight_plugin_schema_node(
    schema: &Value,
    depth: usize,
    max_string_schema_length: usize,
) -> Result<PluginSchemaType, SchemaProfileFailure> {
    if depth > PLUGIN_MAX_JSON_DEPTH {
        return Err(SchemaProfileFailure::invalid(
            "schema exceeds profile nesting bound",
        ));
    }
    let object = schema
        .as_object()
        .ok_or_else(|| SchemaProfileFailure::invalid("schema node must be an object"))?;
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "type"
                | "title"
                | "description"
                | "properties"
                | "required"
                | "additionalProperties"
                | "enum"
                | "const"
                | "minLength"
                | "maxLength"
                | "minItems"
                | "maxItems"
                | "items"
        ) {
            return Err(SchemaProfileFailure::unsupported());
        }
    }

    let schema_type = object
        .get("type")
        .and_then(Value::as_str)
        .and_then(parse_plugin_schema_type)
        .ok_or_else(|| {
            SchemaProfileFailure::invalid("schema type must be one supported scalar type")
        })?;

    for field in ["title", "description"] {
        if let Some(value) = object.get(field) {
            let text = value
                .as_str()
                .ok_or_else(|| SchemaProfileFailure::invalid("schema annotation must be string"))?;
            if validate_text_controls(text, "schema annotation").is_err() {
                return Err(SchemaProfileFailure::invalid(
                    "schema annotation contains unsupported control characters",
                ));
            }
        }
    }

    if let Some(values) = object.get("enum") {
        let values = values
            .as_array()
            .ok_or_else(|| SchemaProfileFailure::invalid("schema enum must be an array"))?;
        if values.is_empty() || values.len() > PLUGIN_SCHEMA_MAX_ENUM_VALUES {
            return Err(SchemaProfileFailure::invalid(
                "schema enum exceeds profile bounds",
            ));
        }
    }

    let has_object_keywords = object.contains_key("properties")
        || object.contains_key("required")
        || object.contains_key("additionalProperties");
    if has_object_keywords && schema_type != PluginSchemaType::Object {
        return Err(SchemaProfileFailure::invalid(
            "object schema keywords require type object",
        ));
    }
    if schema_type == PluginSchemaType::Object {
        let properties = object.get("properties").map(|value| {
            value
                .as_object()
                .ok_or_else(|| SchemaProfileFailure::invalid("properties must be an object"))
        });
        let properties = match properties {
            Some(result) => Some(result?),
            None => None,
        };
        if properties.is_some_and(|values| values.len() > PLUGIN_SCHEMA_MAX_PROPERTIES) {
            return Err(SchemaProfileFailure::invalid(
                "properties exceeds profile bounds",
            ));
        }
        if let Some(properties) = properties {
            for child in properties.values() {
                preflight_plugin_schema_node(child, depth + 1, max_string_schema_length)?;
            }
        }

        if let Some(required) = object.get("required") {
            let required = required
                .as_array()
                .ok_or_else(|| SchemaProfileFailure::invalid("required must be an array"))?;
            if required.len() > PLUGIN_SCHEMA_MAX_REQUIRED {
                return Err(SchemaProfileFailure::invalid(
                    "required exceeds profile bounds",
                ));
            }
            let mut names = HashSet::new();
            for name in required {
                let name = name.as_str().ok_or_else(|| {
                    SchemaProfileFailure::invalid("required entries must be strings")
                })?;
                if !names.insert(name) {
                    return Err(SchemaProfileFailure::invalid(
                        "required entries must be unique",
                    ));
                }
            }
            if object.get("additionalProperties") == Some(&Value::Bool(false)) {
                if let Some(properties) = properties {
                    if names.iter().any(|name| !properties.contains_key(*name)) {
                        return Err(SchemaProfileFailure::invalid(
                            "required property is forbidden by additionalProperties false",
                        ));
                    }
                } else if !names.is_empty() {
                    return Err(SchemaProfileFailure::invalid(
                        "required property is forbidden by additionalProperties false",
                    ));
                }
            }
        }
        if object
            .get("additionalProperties")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err(SchemaProfileFailure::invalid(
                "additionalProperties must be boolean",
            ));
        }
    }

    let has_string_keywords = object.contains_key("minLength") || object.contains_key("maxLength");
    if has_string_keywords && schema_type != PluginSchemaType::String {
        return Err(SchemaProfileFailure::invalid(
            "string schema keywords require type string",
        ));
    }
    if schema_type == PluginSchemaType::String {
        let minimum = bounded_schema_usize(object.get("minLength"), max_string_schema_length)?;
        let maximum = bounded_schema_usize(object.get("maxLength"), max_string_schema_length)?;
        if minimum
            .zip(maximum)
            .is_some_and(|(minimum, maximum)| minimum > maximum)
        {
            return Err(SchemaProfileFailure::invalid(
                "minLength must not exceed maxLength",
            ));
        }
    }

    let has_array_keywords = object.contains_key("minItems")
        || object.contains_key("maxItems")
        || object.contains_key("items");
    if has_array_keywords && schema_type != PluginSchemaType::Array {
        return Err(SchemaProfileFailure::invalid(
            "array schema keywords require type array",
        ));
    }
    if schema_type == PluginSchemaType::Array {
        let minimum = bounded_schema_usize(object.get("minItems"), PLUGIN_MAX_JSON_NODES)?;
        let maximum = bounded_schema_usize(object.get("maxItems"), PLUGIN_MAX_JSON_NODES)?;
        if minimum
            .zip(maximum)
            .is_some_and(|(minimum, maximum)| minimum > maximum)
        {
            return Err(SchemaProfileFailure::invalid(
                "minItems must not exceed maxItems",
            ));
        }
        if let Some(items) = object.get("items") {
            preflight_plugin_schema_node(items, depth + 1, max_string_schema_length)?;
        }
    }

    Ok(schema_type)
}

fn bounded_schema_usize(
    value: Option<&Value>,
    maximum: usize,
) -> Result<Option<usize>, SchemaProfileFailure> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value <= maximum)
        .ok_or_else(|| SchemaProfileFailure::invalid("schema bound is invalid or too large"))?;
    Ok(Some(value))
}

fn validate_plugin_tool_schema_profile(
    schema: &Value,
    field: &str,
    max_string_schema_length: usize,
) -> Result<(), String> {
    if !schema.is_object() {
        return Err(format!("{field} must be a JSON object"));
    }
    validate_json_value(schema, PLUGIN_MAX_SCHEMA_BYTES, field)?;
    preflight_plugin_schema_profile(schema, true, max_string_schema_length)
        .map_err(|failure| format!("{field}: {}", failure.message))
}

pub fn validate_plugin_input_arguments(
    input_schema: &Value,
    arguments: &Value,
) -> Result<(), String> {
    validate_plugin_tool_schema_profile(
        input_schema,
        "tool inputSchema",
        PLUGIN_MAX_ARGUMENT_BYTES,
    )?;
    validate_json_value(arguments, PLUGIN_MAX_ARGUMENT_BYTES, "tool arguments")?;
    if !plugin_schema_matches(input_schema, arguments, 0) {
        return Err("tool arguments do not match the admitted Plugin Schema Profile".to_string());
    }
    Ok(())
}

pub fn validate_plugin_structured_output(
    output_schema: &Value,
    structured_content: &Value,
) -> Result<(), String> {
    validate_plugin_tool_schema_profile(
        output_schema,
        "tool outputSchema",
        PLUGIN_MAX_STRUCTURED_CONTENT_BYTES,
    )?;
    validate_json_value(
        structured_content,
        PLUGIN_MAX_STRUCTURED_CONTENT_BYTES,
        "structuredContent",
    )?;
    if !plugin_schema_matches(output_schema, structured_content, 0) {
        return Err(
            "structuredContent does not match the admitted Plugin Schema Profile".to_string(),
        );
    }
    Ok(())
}

fn plugin_schema_matches(schema: &Value, value: &Value, depth: usize) -> bool {
    if depth > PLUGIN_MAX_JSON_DEPTH {
        return false;
    }
    let Some(object) = schema.as_object() else {
        return false;
    };
    let Some(schema_type) = object
        .get("type")
        .and_then(Value::as_str)
        .and_then(parse_plugin_schema_type)
    else {
        return false;
    };
    let type_matches = match schema_type {
        PluginSchemaType::Object => value.is_object(),
        PluginSchemaType::Array => value.is_array(),
        PluginSchemaType::String => value.is_string(),
        PluginSchemaType::Number => value.is_number(),
        PluginSchemaType::Integer => value
            .as_number()
            .is_some_and(|number| number.as_i64().is_some() || number.as_u64().is_some()),
        PluginSchemaType::Boolean => value.is_boolean(),
        PluginSchemaType::Null => value.is_null(),
    };
    if !type_matches {
        return false;
    }
    if object
        .get("const")
        .is_some_and(|expected| expected != value)
    {
        return false;
    }
    if object
        .get("enum")
        .and_then(Value::as_array)
        .is_some_and(|values| !values.iter().any(|expected| expected == value))
    {
        return false;
    }

    match schema_type {
        PluginSchemaType::Object => {
            let Some(value) = value.as_object() else {
                return false;
            };
            if let Some(required) = object.get("required").and_then(Value::as_array) {
                if required
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|name| !value.contains_key(name))
                {
                    return false;
                }
            }
            let properties = object.get("properties").and_then(Value::as_object);
            let additional = object
                .get("additionalProperties")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            for (name, child_value) in value {
                if let Some(child_schema) = properties.and_then(|properties| properties.get(name)) {
                    if !plugin_schema_matches(child_schema, child_value, depth + 1) {
                        return false;
                    }
                } else if !additional {
                    return false;
                }
            }
        }
        PluginSchemaType::Array => {
            let Some(values) = value.as_array() else {
                return false;
            };
            if object
                .get("minItems")
                .and_then(Value::as_u64)
                .is_some_and(|minimum| values.len() < minimum as usize)
                || object
                    .get("maxItems")
                    .and_then(Value::as_u64)
                    .is_some_and(|maximum| values.len() > maximum as usize)
            {
                return false;
            }
            if let Some(items) = object.get("items") {
                if values
                    .iter()
                    .any(|value| !plugin_schema_matches(items, value, depth + 1))
                {
                    return false;
                }
            }
        }
        PluginSchemaType::String => {
            let Some(text) = value.as_str() else {
                return false;
            };
            let length = text.chars().count();
            if object
                .get("minLength")
                .and_then(Value::as_u64)
                .is_some_and(|minimum| length < minimum as usize)
                || object
                    .get("maxLength")
                    .and_then(Value::as_u64)
                    .is_some_and(|maximum| length > maximum as usize)
            {
                return false;
            }
        }
        PluginSchemaType::Number
        | PluginSchemaType::Integer
        | PluginSchemaType::Boolean
        | PluginSchemaType::Null => {}
    }
    true
}

pub fn validate_tools(tools: &[PluginTool]) -> Result<(), String> {
    if tools.len() > PLUGIN_MAX_TOOL_COUNT {
        return Err(format!(
            "tool count exceeds maximum {}",
            PLUGIN_MAX_TOOL_COUNT
        ));
    }
    let mut names = HashSet::new();
    for tool in tools {
        validate_tool_name(&tool.name)?;
        if !names.insert(tool.name.as_str()) {
            return Err(format!("duplicate tool name '{}'", tool.name));
        }
        if let Some(title) = tool.title.as_deref() {
            if title.is_empty() || title.len() > PLUGIN_MAX_PROVIDER_NAME_BYTES {
                return Err("tool title is empty or too long".to_string());
            }
            validate_text_controls(title, "tool title")?;
        }
        if let Some(description) = tool.description.as_deref() {
            if description.len() > PLUGIN_MAX_DESCRIPTION_BYTES {
                return Err("tool description exceeds plugin bounds".to_string());
            }
            validate_text_controls(description, "tool description")?;
        }
        validate_schema_observation(&tool.schema_observation())?;
    }
    Ok(())
}

/// Return a bounded, stable WebCodex-generated authoring diagnostic for a Tool
/// list that failed `validate_tools`. This helper is diagnostic-only: callers
/// must continue to use `validate_tools` as the authoritative acceptance gate.
pub fn diagnose_invalid_tools(tools: &[PluginTool]) -> PluginCheckDiagnostic {
    fn diagnostic(code: &str, tool: Option<&str>, field: Option<&str>) -> PluginCheckDiagnostic {
        PluginCheckDiagnostic {
            code: code.to_string(),
            tool: tool.map(str::to_string),
            field: field.map(str::to_string),
        }
    }

    fn schema_failure(
        tool: &str,
        field: &'static str,
        value: &Value,
        invalid_code: &'static str,
        max_string_schema_length: Option<usize>,
    ) -> Option<PluginCheckDiagnostic> {
        if !value.is_object() {
            return Some(diagnostic(invalid_code, Some(tool), Some(field)));
        }
        if let Err(error) = validate_json_value(value, PLUGIN_MAX_SCHEMA_BYTES, field) {
            let code = if error.contains("exceeds") || error.contains("oversized") {
                "schema_bounds_exceeded"
            } else {
                invalid_code
            };
            return Some(diagnostic(code, Some(tool), Some(field)));
        }
        if let Some(max_string_schema_length) = max_string_schema_length {
            if let Err(failure) =
                preflight_plugin_schema_profile(value, true, max_string_schema_length)
            {
                let code = match failure.kind {
                    SchemaProfileFailureKind::UnsupportedKeyword => "schema_keyword_unsupported",
                    SchemaProfileFailureKind::Invalid => invalid_code,
                };
                return Some(diagnostic(code, Some(tool), Some(field)));
            }
        }
        None
    }

    if tools.len() > PLUGIN_MAX_TOOL_COUNT {
        return diagnostic("tool_count_exceeded", None, None);
    }
    let mut names = HashSet::new();
    for tool in tools {
        if validate_tool_name(&tool.name).is_err() {
            return diagnostic("invalid_tool_name", None, Some("name"));
        }
        if !names.insert(tool.name.as_str()) {
            return diagnostic("duplicate_tool_name", Some(&tool.name), Some("name"));
        }
        if let Some(title) = tool.title.as_deref() {
            if title.is_empty()
                || title.len() > PLUGIN_MAX_PROVIDER_NAME_BYTES
                || validate_text_controls(title, "tool title").is_err()
            {
                return diagnostic("invalid_tool_title", Some(&tool.name), Some("title"));
            }
        }
        if let Some(description) = tool.description.as_deref() {
            if description.len() > PLUGIN_MAX_DESCRIPTION_BYTES
                || validate_text_controls(description, "tool description").is_err()
            {
                return diagnostic(
                    "invalid_tool_description",
                    Some(&tool.name),
                    Some("description"),
                );
            }
        }
        if let Some(diagnostic) = schema_failure(
            &tool.name,
            "inputSchema",
            &tool.input_schema,
            "input_schema_invalid",
            Some(PLUGIN_MAX_ARGUMENT_BYTES),
        ) {
            return diagnostic;
        }
        if let Some(output) = tool.output_schema.as_ref() {
            if let Some(diagnostic) = schema_failure(
                &tool.name,
                "outputSchema",
                output,
                "output_schema_invalid",
                Some(PLUGIN_MAX_STRUCTURED_CONTENT_BYTES),
            ) {
                return diagnostic;
            }
        }
        if let Some(annotations) = tool.annotations.as_ref() {
            if let Some(diagnostic) = schema_failure(
                &tool.name,
                "annotations",
                annotations,
                "annotations_invalid",
                None,
            ) {
                return diagnostic;
            }
        }
    }
    diagnostic("tool_definition_invalid", None, None)
}

pub fn validate_schema_observation(observation: &PluginSchemaObservation) -> Result<(), String> {
    validate_plugin_tool_schema_profile(
        &observation.input_schema,
        "tool inputSchema",
        PLUGIN_MAX_ARGUMENT_BYTES,
    )?;
    if let Some(output) = observation.output_schema.as_ref() {
        validate_plugin_tool_schema_profile(
            output,
            "tool outputSchema",
            PLUGIN_MAX_STRUCTURED_CONTENT_BYTES,
        )?;
    }
    if let Some(annotations) = observation.annotations.as_ref() {
        if !annotations.is_object() {
            return Err("tool annotations must be a JSON object".to_string());
        }
        validate_json_value(annotations, PLUGIN_MAX_SCHEMA_BYTES, "tool annotations")?;
    }
    Ok(())
}

pub fn validate_tool_result(result: &PluginToolResult) -> Result<(), String> {
    if result.content.len() > PLUGIN_MAX_CONTENT_ITEMS {
        return Err("tool result content item count exceeds bound".to_string());
    }
    let mut text_bytes = 0usize;
    for content in &result.content {
        let PluginContent::Text { text } = content;
        if text.len() > PLUGIN_MAX_TEXT_CONTENT_BYTES {
            return Err("tool result text exceeds bound".to_string());
        }
        validate_text_controls(text, "tool result text")?;
        text_bytes = text_bytes.saturating_add(text.len());
    }
    if let Some(structured) = result.structured_content.as_ref() {
        if !structured.is_object() {
            return Err("structuredContent must be a JSON object".to_string());
        }
        validate_json_value(
            structured,
            PLUGIN_MAX_STRUCTURED_CONTENT_BYTES,
            "structuredContent",
        )?;
    }
    let encoded = serde_json::to_vec(result)
        .map_err(|_| "tool result could not be serialized".to_string())?;
    if encoded.len() > PLUGIN_MAX_RESULT_BYTES || text_bytes > PLUGIN_MAX_RESULT_BYTES {
        return Err("tool result exceeds aggregate bound".to_string());
    }
    Ok(())
}

pub fn validate_response(response: &PluginGatewayResponse) -> Result<(), String> {
    let encoded = serde_json::to_vec(response)
        .map_err(|_| "plugin gateway response could not be serialized".to_string())?;
    if encoded.len() > PLUGIN_MAX_MESSAGE_BYTES {
        return Err("plugin gateway response exceeds message bound".to_string());
    }
    match (&response.payload, &response.error) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(
                "plugin gateway response must contain exactly one payload or error".to_string(),
            )
        }
        (Some(_), None) if response.dispatch_state != PluginDispatchState::Completed => {
            return Err("plugin gateway payload requires completed dispatch".to_string())
        }
        (None, Some(error)) => {
            validate_status_atom(&error.code, "plugin gateway error code")?;
            if error.message.is_empty() || error.message.len() > 512 {
                return Err("plugin gateway error message is invalid".to_string());
            }
            validate_text_controls(&error.message, "plugin gateway error message")?;
        }
        _ => {}
    }
    if let Some(payload) = response.payload.as_ref() {
        match payload {
            PluginGatewayResponsePayload::Checked { report } => validate_check_report(report)?,
            PluginGatewayResponsePayload::Providers { providers } => {
                validate_provider_views(providers)?
            }
            PluginGatewayResponsePayload::ProjectCatalog { catalog } => {
                validate_project_plugin_catalog(catalog)?
            }
            PluginGatewayResponsePayload::Reloaded {
                providers,
                failures,
                ..
            } => {
                validate_provider_views(providers)?;
                if failures.len() > PLUGIN_MAX_PROVIDERS {
                    return Err("plugin reload failure count exceeds bound".to_string());
                }
                for failure in failures {
                    validate_provider_id(&failure.provider_id)?;
                    validate_status_atom(&failure.code, "plugin reload failure code")?;
                }
            }
            PluginGatewayResponsePayload::Tools { tools } => validate_tools(tools)?,
            PluginGatewayResponsePayload::ToolResult { result } => validate_tool_result(result)?,
        }
    }
    Ok(())
}

pub fn validate_response_for_request(
    request: &PluginGatewayRequest,
    response: &PluginGatewayResponse,
) -> Result<(), String> {
    validate_response(response)?;
    if response.error.is_some() {
        return Ok(());
    }
    match (request, response.payload.as_ref()) {
        (
            PluginGatewayRequest::Check { provider_id },
            Some(PluginGatewayResponsePayload::Checked { report }),
        ) => {
            if report.provider_id != *provider_id {
                return Err("Plugin check response provider_id does not match request".to_string());
            }
        }
        (PluginGatewayRequest::Reload, Some(PluginGatewayResponsePayload::Reloaded { .. }))
        | (
            PluginGatewayRequest::ProjectCatalog { .. },
            Some(PluginGatewayResponsePayload::ProjectCatalog { .. }),
        )
        | (
            PluginGatewayRequest::ProvidersList,
            Some(PluginGatewayResponsePayload::Providers { .. }),
        )
        | (
            PluginGatewayRequest::ToolsList { .. },
            Some(PluginGatewayResponsePayload::Tools { .. }),
        )
        | (
            PluginGatewayRequest::ToolsCall { .. },
            Some(PluginGatewayResponsePayload::ToolResult { .. }),
        ) => {}
        _ => {
            return Err("Plugin gateway response kind does not match request operation".to_string())
        }
    }
    Ok(())
}

pub fn validate_project_plugin_catalog(catalog: &ProjectPluginCatalog) -> Result<(), String> {
    validate_project_plugin_catalog_revision(&catalog.catalog_revision)?;
    if catalog.total_count != catalog.entries.len()
        || catalog.entries.len() > PLUGIN_MAX_PROJECT_CATALOG_ENTRIES
    {
        return Err("project Plugin catalog count is invalid".to_string());
    }
    let mut previous: Option<(&str, &str)> = None;
    for entry in &catalog.entries {
        validate_provider_id(&entry.plugin)?;
        validate_provider_name(&entry.name)?;
        validate_tool_name(&entry.tool)?;
        if let Some(title) = entry.title.as_deref() {
            if title.is_empty() || title.len() > PLUGIN_MAX_PROVIDER_NAME_BYTES {
                return Err("project Plugin catalog tool title is invalid".to_string());
            }
            validate_text_controls(title, "project Plugin catalog tool title")?;
        }
        if let Some(description) = entry.description.as_deref() {
            if description.len() > PLUGIN_MAX_PROJECT_CATALOG_DESCRIPTION_BYTES {
                return Err("project Plugin catalog description exceeds bound".to_string());
            }
            validate_text_controls(description, "project Plugin catalog description")?;
        }
        let current = (entry.plugin.as_str(), entry.tool.as_str());
        if previous.is_some_and(|previous| previous >= current) {
            return Err("project Plugin catalog entries are not strictly ordered".to_string());
        }
        previous = Some(current);
    }
    Ok(())
}

pub fn validate_check_report(report: &PluginCheckReport) -> Result<(), String> {
    validate_provider_id(&report.provider_id)?;
    if report.ready {
        if report.phase != PluginCheckPhase::Ready
            || report.code.is_some()
            || report.detail.is_some()
            || report.diagnostic.is_some()
        {
            return Err("ready Plugin check report has inconsistent status fields".to_string());
        }
    } else {
        if report.phase == PluginCheckPhase::Ready || report.code.is_none() {
            return Err("failed Plugin check report has inconsistent status fields".to_string());
        }
        if report.tool_count != 0 || !report.tools.is_empty() {
            return Err("failed Plugin check report must not retain tool inventory".to_string());
        }
        if report.diagnostic.is_some()
            && (report.phase != PluginCheckPhase::Validation
                || report.code.as_deref() != Some("plugin_tools_list_invalid"))
        {
            return Err(
                "Plugin check diagnostic is only valid for tools/list validation failures"
                    .to_string(),
            );
        }
    }
    if let Some(code) = report.code.as_deref() {
        validate_status_atom(code, "Plugin check code")?;
    }
    if let Some(detail) = report.detail.as_deref() {
        if detail.is_empty() || detail.len() > PLUGIN_MAX_CHECK_DETAIL_BYTES {
            return Err("Plugin check detail exceeds bound".to_string());
        }
        validate_text_controls(detail, "Plugin check detail")?;
    }
    if report.tool_count != report.tools.len() || report.tools.len() > PLUGIN_MAX_TOOL_COUNT {
        return Err("Plugin check tool summary count is invalid".to_string());
    }
    for tool in &report.tools {
        validate_tool_name(&tool.name)?;
        if let Some(title) = tool.title.as_deref() {
            if title.is_empty() || title.len() > PLUGIN_MAX_PROVIDER_NAME_BYTES {
                return Err("Plugin check tool title is invalid".to_string());
            }
            validate_text_controls(title, "Plugin check tool title")?;
        }
    }
    if let Some(diagnostic) = report.diagnostic.as_ref() {
        validate_check_diagnostic(diagnostic)?;
    }
    Ok(())
}

fn validate_check_diagnostic(diagnostic: &PluginCheckDiagnostic) -> Result<(), String> {
    let tool = diagnostic.tool.as_deref();
    let field = diagnostic.field.as_deref();
    let validate_tool = |tool: Option<&str>| -> Result<(), String> {
        let tool = tool.ok_or_else(|| "Plugin check diagnostic requires tool".to_string())?;
        validate_tool_name(tool)
    };
    match (diagnostic.code.as_str(), tool, field) {
        (
            "tools_list_result_malformed" | "tool_count_exceeded" | "tool_definition_invalid",
            None,
            None,
        ) => Ok(()),
        ("invalid_tool_name", None, Some("name")) => Ok(()),
        ("duplicate_tool_name", tool, Some("name")) => validate_tool(tool),
        ("invalid_tool_title", tool, Some("title")) => validate_tool(tool),
        ("invalid_tool_description", tool, Some("description")) => validate_tool(tool),
        ("input_schema_invalid", tool, Some("inputSchema")) => validate_tool(tool),
        ("output_schema_invalid", tool, Some("outputSchema")) => validate_tool(tool),
        ("schema_keyword_unsupported", tool, Some("inputSchema" | "outputSchema")) => {
            validate_tool(tool)
        }
        ("annotations_invalid", tool, Some("annotations")) => validate_tool(tool),
        ("schema_bounds_exceeded", tool, Some("inputSchema" | "outputSchema" | "annotations")) => {
            validate_tool(tool)
        }
        _ => Err("Plugin check diagnostic fields are inconsistent with its code".to_string()),
    }
}

fn validate_provider_views(providers: &[PluginProviderView]) -> Result<(), String> {
    if providers.len() > PLUGIN_MAX_PROVIDERS {
        return Err("plugin provider count exceeds bound".to_string());
    }
    let mut ids = HashSet::new();
    for provider in providers {
        validate_provider_id(&provider.provider_id)?;
        validate_provider_instance_id(&provider.provider_instance_id)?;
        validate_provider_name(&provider.name)?;
        validate_status_atom(&provider.status, "plugin provider status")?;
        if let Some(code) = provider.error_code.as_deref() {
            validate_status_atom(code, "plugin provider error code")?;
        }
        if !ids.insert(provider.provider_id.as_str()) {
            return Err("duplicate plugin provider id".to_string());
        }
    }
    Ok(())
}

fn validate_status_atom(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 80
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(format!("{field} is invalid"));
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
    if depth > PLUGIN_MAX_JSON_DEPTH {
        return Err(format!("{field} exceeds JSON depth bound"));
    }
    *nodes = nodes.saturating_add(1);
    if *nodes > PLUGIN_MAX_JSON_NODES {
        return Err(format!("{field} exceeds JSON node bound"));
    }
    match value {
        Value::String(text) => {
            if text.len() > PLUGIN_MAX_JSON_STRING_BYTES {
                return Err(format!("{field} contains an oversized string"));
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_json_node(value, depth + 1, nodes, field)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                if key.len() > 1024 || key.chars().any(char::is_control) {
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

    fn tool() -> PluginTool {
        PluginTool {
            name: "echo".to_string(),
            title: None,
            description: Some("Echo input".to_string()),
            input_schema: json!({"type":"object"}),
            output_schema: None,
            annotations: None,
        }
    }

    #[test]
    fn result_rejects_unsupported_content_at_deserialize_boundary() {
        let value = json!({"content":[{"type":"image","data":"x"}],"isError":false});
        assert!(serde_json::from_value::<PluginToolResult>(value).is_err());
    }

    #[test]
    fn request_requires_object_arguments_and_bounded_schema() {
        let request = PluginGatewayRequest::ToolsCall {
            provider_id: "repo-tools".to_string(),
            provider_instance_id: "instance_1".to_string(),
            name: "echo".to_string(),
            arguments: json!([]),
            expected_schema: tool().schema_observation(),
        };
        assert!(validate_request(&request).is_err());
    }

    #[test]
    fn check_contract_is_typed_and_report_is_bounded() {
        let request = PluginGatewayRequest::Check {
            provider_id: "repo-tools".to_string(),
        };
        validate_request(&request).unwrap();

        let report = PluginCheckReport {
            provider_id: "repo-tools".to_string(),
            ready: true,
            phase: PluginCheckPhase::Ready,
            code: None,
            detail: None,
            tool_count: 1,
            tools: vec![PluginCheckToolSummary {
                name: "echo".to_string(),
                title: Some("Echo".to_string()),
            }],
            diagnostic: None,
        };
        validate_check_report(&report).unwrap();
        validate_response(&PluginGatewayResponse::success(
            PluginGatewayResponsePayload::Checked {
                report: report.clone(),
            },
        ))
        .unwrap();
        validate_response_for_request(
            &request,
            &PluginGatewayResponse::success(PluginGatewayResponsePayload::Checked {
                report: report.clone(),
            }),
        )
        .unwrap();

        let mut wrong_provider = report.clone();
        wrong_provider.provider_id = "other-tools".to_string();
        assert!(validate_response_for_request(
            &request,
            &PluginGatewayResponse::success(PluginGatewayResponsePayload::Checked {
                report: wrong_provider,
            }),
        )
        .is_err());
        assert!(validate_response_for_request(
            &request,
            &PluginGatewayResponse::success(PluginGatewayResponsePayload::Providers {
                providers: vec![],
            }),
        )
        .is_err());

        let mut invalid = report;
        invalid.detail = Some("x".repeat(PLUGIN_MAX_CHECK_DETAIL_BYTES + 1));
        invalid.ready = false;
        invalid.phase = PluginCheckPhase::Validation;
        invalid.code = Some("plugin_tools_list_invalid".to_string());
        invalid.tool_count = 0;
        invalid.tools.clear();
        assert!(validate_check_report(&invalid).is_err());
    }

    #[test]
    fn schema_and_result_bounds_fail_closed() {
        assert_eq!(PLUGIN_MAX_ARGUMENT_BYTES, 64 * 1024);
        assert_eq!(PLUGIN_MAX_SCHEMA_BYTES, 64 * 1024);
        assert_eq!(PLUGIN_MAX_TEXT_CONTENT_BYTES, 512 * 1024);
        assert_eq!(PLUGIN_MAX_STRUCTURED_CONTENT_BYTES, 512 * 1024);
        assert_eq!(PLUGIN_MAX_RESULT_BYTES, 512 * 1024);
        assert_eq!(PLUGIN_MAX_JSON_STRING_BYTES, 512 * 1024);
        assert_eq!(PLUGIN_MAX_MESSAGE_BYTES, 1024 * 1024);

        let mut oversized_schema_tool = tool();
        oversized_schema_tool.input_schema = json!({
            "type": "object",
            "description": "x".repeat(PLUGIN_MAX_SCHEMA_BYTES)
        });
        assert!(validate_tools(&[oversized_schema_tool]).is_err());

        let oversized_arguments = json!({"value": "x".repeat(PLUGIN_MAX_ARGUMENT_BYTES)});
        assert!(validate_json_value(
            &oversized_arguments,
            PLUGIN_MAX_ARGUMENT_BYTES,
            "tool arguments"
        )
        .is_err());

        let mut unreachable_input_schema = tool();
        unreachable_input_schema.input_schema = json!({
            "type": "object",
            "properties": {
                "value": {
                    "type": "string",
                    "minLength": PLUGIN_MAX_ARGUMENT_BYTES + 1
                }
            }
        });
        assert!(validate_tools(&[unreachable_input_schema.clone()]).is_err());
        assert_eq!(
            diagnose_invalid_tools(&[unreachable_input_schema]).code,
            "input_schema_invalid"
        );

        let large_text = PluginToolResult {
            content: vec![PluginContent::Text {
                text: "x".repeat(384 * 1024),
            }],
            structured_content: None,
            is_error: false,
        };
        validate_tool_result(&large_text).unwrap();

        let large_structured = PluginToolResult {
            content: vec![],
            structured_content: Some(json!({"payload": "x".repeat(384 * 1024)})),
            is_error: false,
        };
        validate_tool_result(&large_structured).unwrap();

        let output_schema = json!({
            "type": "object",
            "properties": {
                "payload": {"type": "string", "maxLength": 256 * 1024}
            },
            "required": ["payload"],
            "additionalProperties": false
        });
        let mut large_output_schema_tool = tool();
        large_output_schema_tool.output_schema = Some(output_schema.clone());
        validate_tools(&[large_output_schema_tool]).unwrap();
        validate_plugin_structured_output(
            &output_schema,
            &json!({"payload": "x".repeat(192 * 1024)}),
        )
        .unwrap();
        assert!(validate_plugin_input_arguments(
            &output_schema,
            &json!({"payload": "x".repeat(80 * 1024)}),
        )
        .is_err());

        let aggregate_oversized = PluginToolResult {
            content: vec![PluginContent::Text {
                text: "x".repeat(300 * 1024),
            }],
            structured_content: Some(json!({"payload": "y".repeat(300 * 1024)})),
            is_error: false,
        };
        assert!(validate_tool_result(&aggregate_oversized)
            .unwrap_err()
            .contains("aggregate"));

        let oversized_text = PluginToolResult {
            content: vec![PluginContent::Text {
                text: "x".repeat(PLUGIN_MAX_TEXT_CONTENT_BYTES + 1),
            }],
            structured_content: None,
            is_error: false,
        };
        assert!(validate_tool_result(&oversized_text).is_err());

        let oversized_structured = PluginToolResult {
            content: vec![],
            structured_content: Some(json!({
                "payload": "x".repeat(PLUGIN_MAX_STRUCTURED_CONTENT_BYTES + 1)
            })),
            is_error: false,
        };
        assert!(validate_tool_result(&oversized_structured).is_err());
    }

    #[test]
    fn catalog_digest_is_deterministic_for_canonical_validated_catalog() {
        let mut alpha = tool();
        alpha.name = "alpha".to_string();
        alpha.input_schema = json!({
            "properties": {
                "value": {"maxLength": 8, "type": "string", "minLength": 1}
            },
            "additionalProperties": false,
            "required": ["value"],
            "type": "object"
        });
        let mut beta = tool();
        beta.name = "beta".to_string();

        let first = PluginCatalog::admit(vec![beta.clone(), alpha.clone()]).unwrap();
        let second = PluginCatalog::admit(vec![alpha.clone(), beta.clone()]).unwrap();
        assert_eq!(first.digest(), second.digest());
        assert_eq!(
            first
                .tools()
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );

        beta.description = Some("changed implementation contract".to_string());
        let changed = PluginCatalog::admit(vec![alpha, beta]).unwrap();
        assert_ne!(first.digest(), changed.digest());
        validate_plugin_catalog_digest(first.digest()).unwrap();
    }

    #[test]
    fn plugin_schema_profile_validates_supported_subset_and_instances() {
        let schema = json!({
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["fast", "safe"],
                    "minLength": 4,
                    "maxLength": 4
                },
                "tags": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 2,
                    "items": {"type": "string", "minLength": 1}
                },
                "fixed": {"type": "boolean", "const": true}
            },
            "required": ["mode", "fixed"],
            "additionalProperties": false
        });
        validate_plugin_input_arguments(&schema, &json!({"mode":"fast","fixed":true,"tags":["x"]}))
            .unwrap();
        for invalid in [
            json!({"mode":"slow","fixed":true}),
            json!({"mode":"fast","fixed":false}),
            json!({"mode":"fast","fixed":true,"tags":[]}),
            json!({"mode":"fast","fixed":true,"extra":1}),
        ] {
            assert!(validate_plugin_input_arguments(&schema, &invalid).is_err());
        }

        validate_plugin_structured_output(
            &json!({
                "type":"object",
                "properties":{"count":{"type":"integer"}},
                "required":["count"],
                "additionalProperties":false
            }),
            &json!({"count":2}),
        )
        .unwrap();
    }

    #[test]
    fn unsupported_schema_keyword_and_deep_schema_fail_admission_with_finite_diagnostic() {
        let mut unsupported = tool();
        unsupported.input_schema = json!({
            "type": "object",
            "$ref": "https://example.invalid/secret-schema"
        });
        assert!(validate_tools(&[unsupported.clone()]).is_err());
        let diagnostic = diagnose_invalid_tools(&[unsupported]);
        assert_eq!(diagnostic.code, "schema_keyword_unsupported");
        assert_eq!(diagnostic.tool.as_deref(), Some("echo"));
        assert_eq!(diagnostic.field.as_deref(), Some("inputSchema"));
        let encoded = serde_json::to_string(&diagnostic).unwrap();
        assert!(!encoded.contains("$ref"));
        assert!(!encoded.contains("example.invalid"));

        let mut schema = json!({"type":"string"});
        for _ in 0..=(PLUGIN_MAX_JSON_DEPTH + 1) {
            schema = json!({"type":"object","properties":{"child":schema}});
        }
        let mut deep = tool();
        deep.input_schema = schema;
        assert!(validate_tools(&[deep.clone()]).is_err());
        assert_eq!(
            diagnose_invalid_tools(&[deep]).code,
            "schema_bounds_exceeded"
        );
    }

    #[test]
    fn provider_catalog_is_not_limited_by_removed_outer_mcp_direct_bound() {
        let tools = (0..65)
            .map(|index| PluginTool {
                name: format!("tool_{index}"),
                ..tool()
            })
            .collect::<Vec<_>>();
        let catalog = PluginCatalog::admit(tools).unwrap();
        assert_eq!(catalog.tools().len(), 65);
        assert!(catalog.tools().iter().any(|tool| tool.name == "tool_64"));
    }

    #[test]
    fn tool_validation_diagnostics_use_a_finite_authoring_taxonomy() {
        let mut duplicate = tool();
        duplicate.name = "echo".to_string();
        let duplicate_tools = vec![tool(), duplicate];
        assert!(validate_tools(&duplicate_tools).is_err());
        assert_eq!(
            diagnose_invalid_tools(&duplicate_tools),
            PluginCheckDiagnostic {
                code: "duplicate_tool_name".to_string(),
                tool: Some("echo".to_string()),
                field: Some("name".to_string()),
            }
        );

        let mut invalid_name = tool();
        invalid_name.name = "bad name".to_string();
        assert!(validate_tools(&[invalid_name.clone()]).is_err());
        assert_eq!(
            diagnose_invalid_tools(&[invalid_name]).code,
            "invalid_tool_name"
        );

        let mut invalid_title = tool();
        invalid_title.title = Some(String::new());
        assert!(validate_tools(&[invalid_title.clone()]).is_err());
        assert_eq!(
            diagnose_invalid_tools(&[invalid_title]).code,
            "invalid_tool_title"
        );

        let mut invalid_description = tool();
        invalid_description.description = Some("x".repeat(PLUGIN_MAX_DESCRIPTION_BYTES + 1));
        assert!(validate_tools(&[invalid_description.clone()]).is_err());
        assert_eq!(
            diagnose_invalid_tools(&[invalid_description]).code,
            "invalid_tool_description"
        );

        let mut invalid_input = tool();
        invalid_input.input_schema = json!([]);
        assert!(validate_tools(&[invalid_input.clone()]).is_err());
        let diagnostic = diagnose_invalid_tools(&[invalid_input]);
        assert_eq!(diagnostic.code, "input_schema_invalid");
        assert_eq!(diagnostic.field.as_deref(), Some("inputSchema"));

        let mut invalid_output = tool();
        invalid_output.output_schema = Some(json!([]));
        assert!(validate_tools(&[invalid_output.clone()]).is_err());
        let diagnostic = diagnose_invalid_tools(&[invalid_output]);
        assert_eq!(diagnostic.code, "output_schema_invalid");
        assert_eq!(diagnostic.field.as_deref(), Some("outputSchema"));

        let mut invalid_annotations = tool();
        invalid_annotations.annotations = Some(json!([]));
        assert!(validate_tools(&[invalid_annotations.clone()]).is_err());
        let diagnostic = diagnose_invalid_tools(&[invalid_annotations]);
        assert_eq!(diagnostic.code, "annotations_invalid");
        assert_eq!(diagnostic.field.as_deref(), Some("annotations"));

        let mut oversized_schema = tool();
        oversized_schema.input_schema = json!({
            "type": "object",
            "description": "x".repeat(PLUGIN_MAX_SCHEMA_BYTES)
        });
        assert!(validate_tools(&[oversized_schema.clone()]).is_err());
        assert_eq!(
            diagnose_invalid_tools(&[oversized_schema]).code,
            "schema_bounds_exceeded"
        );

        let too_many = (0..=PLUGIN_MAX_TOOL_COUNT)
            .map(|index| PluginTool {
                name: format!("tool_{index}"),
                ..tool()
            })
            .collect::<Vec<_>>();
        assert!(validate_tools(&too_many).is_err());
        assert_eq!(
            diagnose_invalid_tools(&too_many).code,
            "tool_count_exceeded"
        );
    }

    #[test]
    fn check_diagnostic_validation_rejects_semantically_impossible_combinations() {
        for diagnostic in [
            PluginCheckDiagnostic {
                code: "tool_count_exceeded".to_string(),
                tool: Some("echo".to_string()),
                field: None,
            },
            PluginCheckDiagnostic {
                code: "invalid_tool_name".to_string(),
                tool: Some("echo".to_string()),
                field: Some("name".to_string()),
            },
            PluginCheckDiagnostic {
                code: "duplicate_tool_name".to_string(),
                tool: Some("echo".to_string()),
                field: Some("title".to_string()),
            },
            PluginCheckDiagnostic {
                code: "schema_bounds_exceeded".to_string(),
                tool: Some("echo".to_string()),
                field: Some("name".to_string()),
            },
        ] {
            assert!(validate_check_diagnostic(&diagnostic).is_err());
        }

        assert!(validate_check_diagnostic(&PluginCheckDiagnostic {
            code: "schema_bounds_exceeded".to_string(),
            tool: Some("echo".to_string()),
            field: Some("inputSchema".to_string()),
        })
        .is_ok());
    }
}
