use super::protocol::request_client_capabilities;
use super::response::{
    mcp_runtime_tool_result_fallback, mcp_stateless_result, rpc_error, rpc_result,
};
use super::{require_mcp_scope, scope_forbidden, McpOutcome};
use crate::auth::AuthContext;
use crate::model_surface::ModelSurface;
use crate::tool_runtime::{
    validate_project_artifact_export_snapshot, ProjectArtifactExportSnapshot, ToolResult,
    ToolRuntime, MAX_PROJECT_ARTIFACT_EXPORT_BYTES, MAX_READ_PROJECT_ARTIFACT_LENGTH,
};
use base64::{engine::general_purpose, Engine as _};
use futures_util::future::join_all;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

pub(super) const MCP_ARTIFACT_EXPORT_URI_PREFIX: &str = "webcodex-artifact://export/";
pub(super) const MCP_ARTIFACT_EXPORT_ID_PREFIX: &str = "wc_export_";
pub(super) const MCP_SNAPSHOT_RESOURCE_URI_PREFIX: &str = "webcodex-snapshot://view/";
pub(super) const MCP_SNAPSHOT_RESOURCE_ID_PREFIX: &str = "wc_snapshot_";
pub(super) const MCP_SNAPSHOT_RESOURCE_TTL: Duration = Duration::from_secs(5 * 60);
pub(super) const MAX_MCP_SNAPSHOT_RESOURCES: usize = 32;
pub(super) const MAX_MCP_SNAPSHOT_RESOURCES_PER_CALLER: usize = 8;
pub(super) const MCP_ARTIFACT_EXPORT_TTL: Duration = Duration::from_secs(5 * 60);
pub(super) const MCP_ARTIFACT_EXPORT_ADMISSION_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const MCP_ARTIFACT_EXPORT_READ_TIMEOUT: Duration = Duration::from_secs(120);
pub(super) const MCP_ARTIFACT_EXPORT_STREAM_TIMEOUT: Duration = Duration::from_secs(10 * 60);
pub(super) const MAX_MCP_ARTIFACT_EXPORT_READS: usize = 2;
pub(super) const MAX_MCP_ARTIFACT_EXPORT_CHUNK_READS: usize = 4;
pub(super) const MAX_MCP_ARTIFACT_EXPORTS: usize = 128;
pub(super) const MAX_MCP_ARTIFACT_EXPORTS_PER_CALLER: usize = 16;
pub(super) const MCP_ARTIFACT_EXPORT_BUSY_CODE: i64 = -32029;
pub(super) const MCP_UI_EXTENSION: &str = "io.modelcontextprotocol/ui";
pub(super) const MCP_COMPUTER_UI_RESOURCE_URI: &str = "ui://webcodex/computer/v12";
pub(super) const MCP_COMPUTER_UI_RESOURCE_LEGACY_URIS: &[&str] = &[
    "ui://webcodex/computer/v1",
    "ui://webcodex/computer/v2",
    "ui://webcodex/computer/v3",
    "ui://webcodex/computer/v4",
    "ui://webcodex/computer/v5",
    "ui://webcodex/computer/v6",
    "ui://webcodex/computer/v7",
    "ui://webcodex/computer/v8",
    "ui://webcodex/computer/v9",
    "ui://webcodex/computer/v10",
    "ui://webcodex/computer/v11",
];
// Temporary gray-card diagnostic: force the host to re-read the canonical App
// resource for every card so resource reuse/cache is not an unobserved variable.
pub(super) const MCP_COMPUTER_UI_RESOURCE_TTL_MS: u64 = 0;
pub(super) const MCP_RESULT_UI_RESOURCE_URI: &str = "ui://webcodex/result/v3";
pub(super) const MCP_RESULT_UI_RESOURCE_LEGACY_URIS: &[&str] =
    &["ui://webcodex/result/v1", "ui://webcodex/result/v2"];
pub(super) const MCP_UI_RESOURCE_MIME_TYPE: &str = "text/html;profile=mcp-app";
pub(super) const MCP_COMPUTER_APP_HTML: &str = include_str!("../mcp_computer_app.html");
pub(super) const MCP_RESULT_APP_HTML: &str = include_str!("../mcp_result_app.html");

pub(super) fn request_supports_mcp_apps(params: &Value) -> bool {
    let Some(extension) = request_client_capabilities(params)
        .and_then(|capabilities| capabilities.get("extensions"))
        .and_then(|extensions| extensions.get(MCP_UI_EXTENSION))
        .and_then(Value::as_object)
    else {
        return false;
    };
    match extension.get("mimeTypes").and_then(Value::as_array) {
        Some(mime_types) => mime_types
            .iter()
            .any(|mime| mime.as_str() == Some(MCP_UI_RESOURCE_MIME_TYPE)),
        None => false,
    }
}

pub(super) fn model_surface_supports_mcp_apps(model_surface: ModelSurface) -> bool {
    model_surface.supports_operator_extensions()
}

pub(super) fn model_surface_supports_computer_app(model_surface: ModelSurface) -> bool {
    model_surface_supports_mcp_apps(model_surface)
}

fn mcp_app_resource_meta(domain: Option<&str>) -> Value {
    let mut ui = json!({
        "prefersBorder": true,
        "csp": {
            "connectDomains": [],
            "resourceDomains": []
        }
    });
    if let Some(domain) = domain {
        ui["domain"] = Value::String(domain.to_string());
    }
    json!({ "ui": ui })
}

pub(super) fn mcp_computer_app_resource_meta(domain: Option<&str>) -> Value {
    mcp_app_resource_meta(domain)
}

pub(super) fn mcp_computer_app_resources_list(domain: Option<&str>) -> Value {
    json!({
        "resources": [{
            "uri": MCP_COMPUTER_UI_RESOURCE_URI,
            "name": "WebCodex Computer",
            "description": "Minimal read-only WebCodex Computer screenshot card that performs only the standard MCP Apps handshake and renders the native computer_snapshot image.",
            "mimeType": MCP_UI_RESOURCE_MIME_TYPE,
            "_meta": mcp_computer_app_resource_meta(domain)
        }]
    })
}

pub(super) fn mcp_result_app_resource_meta(domain: Option<&str>) -> Value {
    mcp_app_resource_meta(domain)
}

pub(super) fn mcp_app_resources_list(domain: Option<&str>) -> Value {
    let mut result = mcp_computer_app_resources_list(domain);
    result["resources"]
        .as_array_mut()
        .expect("computer App resource list must be an array")
        .push(json!({
            "uri": MCP_RESULT_UI_RESOURCE_URI,
            "name": "WebCodex Result",
            "description": "Read-only bounded WebCodex presentation for Jobs, validation evidence, and Git summaries. The App renders allowlisted presentation metadata and never performs tool calls or owns runtime state.",
            "mimeType": MCP_UI_RESOURCE_MIME_TYPE,
            "_meta": mcp_result_app_resource_meta(domain)
        }));
    result
}

pub(super) fn is_mcp_computer_app_resource_uri(uri: &str) -> bool {
    uri == MCP_COMPUTER_UI_RESOURCE_URI || MCP_COMPUTER_UI_RESOURCE_LEGACY_URIS.contains(&uri)
}

pub(super) fn mcp_computer_app_resource_read(uri: &str, domain: Option<&str>) -> Option<Value> {
    // ChatGPT can retain an older tool descriptor across connector refreshes.
    // Keep prior computer App URIs as hidden read aliases so an already-bound
    // card can fetch the current safe template. resources/list and tools/list
    // still advertise only the canonical URI above.
    let supported = is_mcp_computer_app_resource_uri(uri);
    supported.then(|| {
        json!({
            "contents": [{
                "uri": uri,
                "mimeType": MCP_UI_RESOURCE_MIME_TYPE,
                "text": MCP_COMPUTER_APP_HTML,
                "_meta": mcp_computer_app_resource_meta(domain)
            }]
        })
    })
}

pub(super) fn is_mcp_result_app_resource_uri(uri: &str) -> bool {
    uri == MCP_RESULT_UI_RESOURCE_URI || MCP_RESULT_UI_RESOURCE_LEGACY_URIS.contains(&uri)
}

pub(super) fn mcp_result_app_resource_read(uri: &str, domain: Option<&str>) -> Option<Value> {
    is_mcp_result_app_resource_uri(uri).then(|| {
        json!({
            "contents": [{
                "uri": uri,
                "mimeType": MCP_UI_RESOURCE_MIME_TYPE,
                "text": MCP_RESULT_APP_HTML,
                "_meta": mcp_result_app_resource_meta(domain)
            }]
        })
    })
}

fn mcp_static_app_resource_read(uri: &str, domain: Option<&str>) -> Option<Value> {
    mcp_computer_app_resource_read(uri, domain)
        .or_else(|| mcp_result_app_resource_read(uri, domain))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum McpArtifactExportCallerBinding {
    Bootstrap,
    ApiToken {
        api_key_id: String,
    },
    AgentToken {
        api_key_id: String,
    },
    AccountCredential {
        user_id: String,
    },
    OAuthUser {
        user_id: String,
        client_id: String,
    },
    OAuthSharedKey {
        shared_key_hash: String,
        client_id: String,
    },
    SharedKey {
        shared_key_hash: String,
    },
    ProjectCredential {
        project_grant_id: String,
    },
}

pub(super) fn mcp_artifact_export_caller_binding(
    auth: Option<&AuthContext>,
) -> Result<McpArtifactExportCallerBinding, &'static str> {
    let auth = auth.ok_or("authenticated caller identity is unavailable")?;
    match auth.kind {
        crate::auth::AuthKind::Bootstrap => Ok(McpArtifactExportCallerBinding::Bootstrap),
        crate::auth::AuthKind::ApiToken => auth
            .api_key_id
            .as_ref()
            .filter(|value| !value.is_empty())
            .cloned()
            .map(|api_key_id| McpArtifactExportCallerBinding::ApiToken { api_key_id })
            .ok_or("API token identity is unavailable"),
        crate::auth::AuthKind::AgentToken => auth
            .api_key_id
            .as_ref()
            .filter(|value| !value.is_empty())
            .cloned()
            .map(|api_key_id| McpArtifactExportCallerBinding::AgentToken { api_key_id })
            .ok_or("agent token identity is unavailable"),
        crate::auth::AuthKind::AccountCredential => auth
            .user_id
            .as_ref()
            .filter(|value| !value.is_empty())
            .cloned()
            .map(|user_id| McpArtifactExportCallerBinding::AccountCredential { user_id })
            .ok_or("account identity is unavailable"),
        crate::auth::AuthKind::OAuth2Token => {
            let client_id = auth
                .allowed_client_id
                .as_ref()
                .filter(|value| !value.is_empty())
                .cloned()
                .ok_or("OAuth client identity is unavailable")?;
            if auth.is_oauth_shared_key_subject() {
                let shared_key_hash = auth
                    .shared_key_hash
                    .as_ref()
                    .filter(|value| !value.is_empty())
                    .cloned()
                    .ok_or("OAuth shared-key subject identity is unavailable")?;
                Ok(McpArtifactExportCallerBinding::OAuthSharedKey {
                    shared_key_hash,
                    client_id,
                })
            } else if auth.token_kind.as_deref() == Some("oauth2") {
                let user_id = auth
                    .user_id
                    .as_ref()
                    .filter(|value| !value.is_empty())
                    .cloned()
                    .ok_or("OAuth user identity is unavailable")?;
                Ok(McpArtifactExportCallerBinding::OAuthUser { user_id, client_id })
            } else {
                Err("unsupported OAuth subject identity")
            }
        }
        crate::auth::AuthKind::SharedKey => auth
            .shared_key_hash
            .as_ref()
            .filter(|value| !value.is_empty())
            .cloned()
            .map(|shared_key_hash| McpArtifactExportCallerBinding::SharedKey { shared_key_hash })
            .ok_or("shared-key identity is unavailable"),
        crate::auth::AuthKind::ProjectCredential => auth
            .project_grant_id
            .as_ref()
            .filter(|value| !value.is_empty())
            .cloned()
            .map(
                |project_grant_id| McpArtifactExportCallerBinding::ProjectCredential {
                    project_grant_id,
                },
            )
            .ok_or("project credential identity is unavailable"),
        crate::auth::AuthKind::OpenAnonymous => {
            Err("anonymous MCP callers cannot create artifact export resources")
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct McpArtifactExportRecord {
    pub(super) caller: McpArtifactExportCallerBinding,
    pub(super) project: String,
    pub(super) snapshot: ProjectArtifactExportSnapshot,
    pub(super) expires_at: Instant,
}

#[derive(Default)]
pub(super) struct McpArtifactExportRegistry {
    pub(super) entries: HashMap<String, McpArtifactExportRecord>,
    pub(super) order: VecDeque<String>,
}

impl McpArtifactExportRegistry {
    pub(super) fn cleanup(&mut self, now: Instant) {
        self.entries.retain(|_, record| record.expires_at > now);
        self.order.retain(|id| self.entries.contains_key(id));
    }

    pub(super) fn insert(&mut self, record: McpArtifactExportRecord) -> String {
        self.cleanup(Instant::now());
        while self
            .entries
            .values()
            .filter(|existing| existing.caller == record.caller)
            .count()
            >= MAX_MCP_ARTIFACT_EXPORTS_PER_CALLER
        {
            let Some(position) = self.order.iter().position(|id| {
                self.entries
                    .get(id)
                    .is_some_and(|existing| existing.caller == record.caller)
            }) else {
                break;
            };
            if let Some(id) = self.order.remove(position) {
                self.entries.remove(&id);
            }
        }
        while self.entries.len() >= MAX_MCP_ARTIFACT_EXPORTS {
            if let Some(id) = self.order.pop_front() {
                self.entries.remove(&id);
            } else {
                break;
            }
        }
        let id = loop {
            let candidate = format!(
                "{MCP_ARTIFACT_EXPORT_ID_PREFIX}{}",
                uuid::Uuid::new_v4().simple()
            );
            if !self.entries.contains_key(&candidate) {
                break candidate;
            }
        };
        self.order.push_back(id.clone());
        self.entries.insert(id.clone(), record);
        format!("{MCP_ARTIFACT_EXPORT_URI_PREFIX}{id}")
    }

    pub(super) fn get_for_caller(
        &mut self,
        uri: &str,
        caller: &McpArtifactExportCallerBinding,
    ) -> Option<McpArtifactExportRecord> {
        self.cleanup(Instant::now());
        let id = mcp_artifact_export_id_from_uri(uri)?;
        self.entries
            .get(id)
            .filter(|record| &record.caller == caller)
            .cloned()
    }
}

pub(super) static MCP_ARTIFACT_EXPORT_REGISTRY: OnceLock<Mutex<McpArtifactExportRegistry>> =
    OnceLock::new();
pub(super) static MCP_ARTIFACT_EXPORT_READ_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();

pub(super) fn mcp_artifact_export_registry() -> &'static Mutex<McpArtifactExportRegistry> {
    MCP_ARTIFACT_EXPORT_REGISTRY.get_or_init(|| Mutex::new(McpArtifactExportRegistry::default()))
}

pub(super) fn mcp_artifact_export_read_semaphore() -> Arc<Semaphore> {
    MCP_ARTIFACT_EXPORT_READ_SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(MAX_MCP_ARTIFACT_EXPORT_READS)))
        .clone()
}

pub(super) fn mcp_artifact_export_id_from_uri(uri: &str) -> Option<&str> {
    let id = uri.strip_prefix(MCP_ARTIFACT_EXPORT_URI_PREFIX)?;
    let hex = id.strip_prefix(MCP_ARTIFACT_EXPORT_ID_PREFIX)?;
    (hex.len() == 32
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    .then_some(id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum McpSnapshotResourceKind {
    Window,
    Display,
}

impl McpSnapshotResourceKind {
    pub(super) fn from_tool_name(tool_name: &str) -> Option<Self> {
        match tool_name {
            "computer_snapshot" => Some(Self::Window),
            "computer_snapshot_display" => Some(Self::Display),
            _ => None,
        }
    }

    pub(super) fn name(self, client_id: &str, mime_type: &str) -> String {
        let extension = match mime_type {
            "image/png" => "png",
            "image/webp" => "webp",
            _ => "jpg",
        };
        let kind = match self {
            Self::Window => "window",
            Self::Display => "display",
        };
        let normalized_client_id: String = client_id
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        let client_id = if normalized_client_id.is_empty() {
            "computer"
        } else {
            normalized_client_id.as_str()
        };
        format!("{client_id}-{kind}-snapshot.{extension}")
    }
}

#[derive(Debug, Clone)]
pub(super) struct McpSnapshotResourceRecord {
    pub(super) caller: McpArtifactExportCallerBinding,
    pub(super) kind: McpSnapshotResourceKind,
    pub(super) bytes: Arc<[u8]>,
    pub(super) mime_type: String,
    pub(super) expires_at: Instant,
}

#[derive(Default)]
pub(super) struct McpSnapshotResourceRegistry {
    pub(super) entries: HashMap<String, McpSnapshotResourceRecord>,
    pub(super) order: VecDeque<String>,
}

impl McpSnapshotResourceRegistry {
    pub(super) fn cleanup(&mut self, now: Instant) {
        self.entries.retain(|_, record| record.expires_at > now);
        self.order.retain(|id| self.entries.contains_key(id));
    }

    pub(super) fn insert(&mut self, record: McpSnapshotResourceRecord) -> String {
        self.cleanup(Instant::now());
        while self
            .entries
            .values()
            .filter(|existing| existing.caller == record.caller)
            .count()
            >= MAX_MCP_SNAPSHOT_RESOURCES_PER_CALLER
        {
            let Some(position) = self.order.iter().position(|id| {
                self.entries
                    .get(id)
                    .is_some_and(|existing| existing.caller == record.caller)
            }) else {
                break;
            };
            if let Some(id) = self.order.remove(position) {
                self.entries.remove(&id);
            }
        }
        while self.entries.len() >= MAX_MCP_SNAPSHOT_RESOURCES {
            if let Some(id) = self.order.pop_front() {
                self.entries.remove(&id);
            } else {
                break;
            }
        }
        let id = loop {
            let candidate = format!(
                "{MCP_SNAPSHOT_RESOURCE_ID_PREFIX}{}",
                uuid::Uuid::new_v4().simple()
            );
            if !self.entries.contains_key(&candidate) {
                break candidate;
            }
        };
        self.order.push_back(id.clone());
        self.entries.insert(id.clone(), record);
        format!("{MCP_SNAPSHOT_RESOURCE_URI_PREFIX}{id}")
    }

    pub(super) fn get_for_caller(
        &mut self,
        uri: &str,
        caller: &McpArtifactExportCallerBinding,
    ) -> Option<McpSnapshotResourceRecord> {
        self.cleanup(Instant::now());
        let id = mcp_snapshot_resource_id_from_uri(uri)?;
        self.entries
            .get(id)
            .filter(|record| &record.caller == caller)
            .cloned()
    }
}

pub(super) static MCP_SNAPSHOT_RESOURCE_REGISTRY: OnceLock<Mutex<McpSnapshotResourceRegistry>> =
    OnceLock::new();

pub(super) fn mcp_snapshot_resource_registry() -> &'static Mutex<McpSnapshotResourceRegistry> {
    MCP_SNAPSHOT_RESOURCE_REGISTRY
        .get_or_init(|| Mutex::new(McpSnapshotResourceRegistry::default()))
}

pub(super) fn mcp_snapshot_resource_id_from_uri(uri: &str) -> Option<&str> {
    let id = uri.strip_prefix(MCP_SNAPSHOT_RESOURCE_URI_PREFIX)?;
    let hex = id.strip_prefix(MCP_SNAPSHOT_RESOURCE_ID_PREFIX)?;
    (hex.len() == 32
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    .then_some(id)
}

pub(super) fn mcp_issue_artifact_export(
    caller: McpArtifactExportCallerBinding,
    result: &ToolResult,
) -> Result<(String, ProjectArtifactExportSnapshot), String> {
    let project = result
        .output
        .get("project")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "export result is missing canonical project identity".to_string())?
        .to_string();
    let path = result
        .output
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "export result is missing artifact path".to_string())?;
    let snapshot = validate_project_artifact_export_snapshot(path, &result.output)?;
    if result.output.get("name").and_then(Value::as_str) != Some(snapshot.name.as_str()) {
        return Err(
            "export result basename does not match validated artifact metadata".to_string(),
        );
    }
    let record = McpArtifactExportRecord {
        caller,
        project,
        snapshot: snapshot.clone(),
        expires_at: Instant::now() + MCP_ARTIFACT_EXPORT_TTL,
    };
    let uri = mcp_artifact_export_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(record);
    Ok((uri, snapshot))
}

pub(super) fn mcp_artifact_export_tool_result(
    result: ToolResult,
    caller: McpArtifactExportCallerBinding,
) -> Value {
    if !result.success {
        return mcp_runtime_tool_result_fallback(result);
    }
    let (uri, snapshot) = match mcp_issue_artifact_export(caller, &result) {
        Ok(value) => value,
        Err(error) => {
            return mcp_runtime_tool_result_fallback(ToolResult::err(format!(
                "cannot frame artifact export resource: {error}"
            )))
        }
    };
    json!({
        "content": [{
            "type": "resource_link",
            "uri": uri,
            "name": snapshot.name,
            "mimeType": snapshot.mime_type,
            "description": "Short-lived authenticated WebCodex project artifact export. Read this URI with MCP resources/read to retrieve the complete bounded binary."
        }],
        "structuredContent": {
            "success": true,
            "output": result.output,
            "error": Value::Null,
        },
        "isError": false
    })
}

#[cfg(test)]
pub(super) fn mcp_expire_artifact_export_for_test(uri: &str) {
    if let Some(id) = mcp_artifact_export_id_from_uri(uri) {
        let mut registry = mcp_artifact_export_registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(record) = registry.entries.get_mut(id) {
            record.expires_at = Instant::now();
        }
    }
}

pub(super) fn mcp_runtime_tool_result_with_snapshot_resource(
    tool_name: &str,
    as_image_requested: bool,
    mut result: ToolResult,
    snapshot_caller: Option<McpArtifactExportCallerBinding>,
) -> Value {
    let native_image_requested = (tool_name == "read_project_artifact" && as_image_requested)
        || matches!(tool_name, "computer_snapshot" | "computer_snapshot_display");
    if native_image_requested && result.success {
        match mcp_native_image_tool_result(tool_name, &mut result, snapshot_caller) {
            Ok(value) => return value,
            Err(error) => {
                result = ToolResult::err(format!(
                    "cannot frame {tool_name} as MCP image content: {error}"
                ));
            }
        }
    }

    mcp_runtime_tool_result_fallback(result)
}

pub(super) fn mcp_native_image_tool_result(
    tool_name: &str,
    result: &mut ToolResult,
    snapshot_caller: Option<McpArtifactExportCallerBinding>,
) -> Result<Value, String> {
    let data = result
        .output
        .get("content_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing content_base64".to_string())?
        .to_string();
    let mime_type = result
        .output
        .get("mime_type")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing mime_type".to_string())?
        .to_string();
    if !matches!(
        mime_type.as_str(),
        "image/png" | "image/jpeg" | "image/webp"
    ) {
        return Err(format!("unsupported MIME type '{mime_type}'"));
    }
    let decoded = general_purpose::STANDARD
        .decode(&data)
        .map_err(|error| format!("invalid image base64: {error}"))?;
    if decoded.is_empty() || decoded.len() > crate::artifact_policy::MAX_MCP_IMAGE_BYTES {
        return Err(format!(
            "image payload exceeds {} decoded bytes",
            crate::artifact_policy::MAX_MCP_IMAGE_BYTES
        ));
    }
    let detected = if decoded.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if decoded.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if decoded.len() >= 12 && decoded.starts_with(b"RIFF") && &decoded[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    };
    if detected != Some(mime_type.as_str()) {
        return Err("image MIME does not match decoded content".to_string());
    }
    let image_label = if tool_name == "computer_snapshot" {
        result
            .output
            .pointer("/surface/surface_id")
            .and_then(Value::as_str)
            .unwrap_or("desktop surface")
    } else if tool_name == "computer_snapshot_display" {
        result
            .output
            .get("display_id")
            .and_then(Value::as_str)
            .unwrap_or("full display")
    } else {
        result
            .output
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("project image")
    };
    let file_bytes = result
        .output
        .get("file_bytes")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing file_bytes".to_string())?;
    if file_bytes != decoded.len() as u64 {
        return Err("file_bytes does not match decoded image payload".to_string());
    }
    let sha256 = result
        .output
        .get("sha256")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let metadata_text = if matches!(tool_name, "computer_snapshot" | "computer_snapshot_display") {
        let width = result
            .output
            .get("width")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let height = result
            .output
            .get("height")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if width == 0 || height == 0 || width > 4096 || height > 4096 {
            return Err("computer snapshot dimensions are invalid".to_string());
        }
        format!("Image {image_label}: {mime_type}, {width}x{height}, {file_bytes} bytes.")
    } else {
        format!("Image {image_label}: {mime_type}, {file_bytes} bytes, sha256 {sha256}.")
    };

    let output = result
        .output
        .as_object_mut()
        .ok_or_else(|| "tool output is not an object".to_string())?;
    output.remove("content_base64");
    output.insert("content_delivery".to_string(), json!("mcp_image"));
    let structured_output = result.output.clone();

    let snapshot_link = snapshot_caller
        .zip(McpSnapshotResourceKind::from_tool_name(tool_name))
        .map(|(caller, kind)| {
            let client_id = result
                .output
                .get("client_id")
                .and_then(Value::as_str)
                .unwrap_or("computer");
            let name = kind.name(client_id, &mime_type);
            let record = McpSnapshotResourceRecord {
                caller,
                kind,
                bytes: Arc::from(decoded.into_boxed_slice()),
                mime_type: mime_type.clone(),
                expires_at: Instant::now() + MCP_SNAPSHOT_RESOURCE_TTL,
            };
            let uri = mcp_snapshot_resource_registry()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(record);
            tracing::info!(
                target: "webcodex::mcp",
                tool_name,
                file_bytes,
                "mcp_snapshot_resource_link_issued"
            );
            json!({
                "type": "resource_link",
                "uri": uri,
                "name": name,
                "mimeType": mime_type,
                "size": file_bytes,
                "description": "Short-lived authenticated WebCodex computer screenshot. No project artifact was created."
            })
        });
    let mut content = Vec::with_capacity(if snapshot_link.is_some() { 3 } else { 2 });
    if let Some(link) = snapshot_link {
        content.push(link);
    }
    content.push(json!({ "type": "text", "text": metadata_text }));
    content.push(json!({ "type": "image", "data": data, "mimeType": mime_type }));

    Ok(json!({
        "content": content,
        "structuredContent": {
            "success": true,
            "output": structured_output,
            "error": Value::Null,
        },
        "isError": false
    }))
}

#[derive(Debug)]
pub(super) enum McpArtifactExportReadError {
    Unavailable,
    Forbidden {
        required_scope: Option<&'static str>,
        description: String,
    },
    SnapshotChanged,
    Unsafe,
    Busy,
    Timeout,
}

pub(super) fn mcp_artifact_export_lookup(
    uri: &str,
    auth: Option<&AuthContext>,
) -> Result<McpArtifactExportRecord, McpArtifactExportReadError> {
    let caller = mcp_artifact_export_caller_binding(auth)
        .map_err(|_| McpArtifactExportReadError::Unavailable)?;
    mcp_artifact_export_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_for_caller(uri, &caller)
        .ok_or(McpArtifactExportReadError::Unavailable)
}

pub(super) async fn mcp_artifact_export_metadata_recheck(
    runtime: &ToolRuntime,
    record: &McpArtifactExportRecord,
    auth: Option<&AuthContext>,
) -> Result<ProjectArtifactExportSnapshot, McpArtifactExportReadError> {
    let result = runtime
        .read_project_artifact_export_metadata_internal(
            &record.project,
            &record.snapshot.path,
            auth,
        )
        .await;
    if !result.success {
        return Err(McpArtifactExportReadError::Unavailable);
    }
    let snapshot = validate_project_artifact_export_snapshot(&record.snapshot.path, &result.output)
        .map_err(|_| McpArtifactExportReadError::Unsafe)?;
    if snapshot != record.snapshot {
        return Err(McpArtifactExportReadError::SnapshotChanged);
    }
    Ok(snapshot)
}

pub(super) fn mcp_artifact_export_decode_chunk(
    record: &McpArtifactExportRecord,
    offset: usize,
    length: usize,
    output: &Value,
) -> Result<Vec<u8>, McpArtifactExportReadError> {
    if output.get("error_kind").and_then(Value::as_str) == Some("snapshot_changed") {
        return Err(McpArtifactExportReadError::SnapshotChanged);
    }
    if output.get("error").and_then(Value::as_str).is_some() {
        return Err(McpArtifactExportReadError::Unsafe);
    }
    if output.get("path").and_then(Value::as_str) != Some(record.snapshot.path.as_str())
        || output.get("file_bytes").and_then(Value::as_u64) != Some(record.snapshot.bytes as u64)
    {
        return Err(McpArtifactExportReadError::SnapshotChanged);
    }
    if output.get("offset").and_then(Value::as_u64) != Some(offset as u64) {
        return Err(McpArtifactExportReadError::Unsafe);
    }
    let encoded = output
        .get("content_base64")
        .and_then(Value::as_str)
        .ok_or(McpArtifactExportReadError::Unsafe)?;
    let decoded = general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| McpArtifactExportReadError::Unsafe)?;
    let bytes_returned = output
        .get("bytes_returned")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(McpArtifactExportReadError::Unsafe)?;
    let next_offset = output
        .get("next_offset")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(McpArtifactExportReadError::Unsafe)?;
    let eof = output
        .get("eof")
        .and_then(Value::as_bool)
        .ok_or(McpArtifactExportReadError::Unsafe)?;
    let truncated = output
        .get("truncated")
        .and_then(Value::as_bool)
        .ok_or(McpArtifactExportReadError::Unsafe)?;
    let expected_next = offset
        .checked_add(decoded.len())
        .ok_or(McpArtifactExportReadError::Unsafe)?;
    if decoded.len() != bytes_returned
        || decoded.len() > length
        || expected_next != next_offset
        || next_offset > record.snapshot.bytes
        || (decoded.is_empty() && offset < record.snapshot.bytes)
        || eof != (next_offset == record.snapshot.bytes)
        || truncated == eof
    {
        return Err(McpArtifactExportReadError::Unsafe);
    }
    Ok(decoded)
}

pub(super) async fn mcp_artifact_export_read_chunk(
    runtime: &ToolRuntime,
    record: &McpArtifactExportRecord,
    auth: Option<&AuthContext>,
    offset: usize,
    length: usize,
) -> Result<Vec<u8>, McpArtifactExportReadError> {
    let output = runtime
        .read_project_artifact_export_chunk_internal(
            &record.project,
            &record.snapshot.path,
            record.snapshot.bytes,
            offset,
            length,
            auth,
        )
        .await
        .map_err(|_| McpArtifactExportReadError::Unavailable)?;
    mcp_artifact_export_decode_chunk(record, offset, length, &output)
}

#[derive(Debug)]
pub(super) struct McpArtifactExportStreamPlan {
    uri: String,
    record: McpArtifactExportRecord,
    first_chunk: Vec<u8>,
    offset: usize,
    chunks: usize,
    max_chunks: usize,
    read_budget: Duration,
    _permit: OwnedSemaphorePermit,
}

pub(super) async fn mcp_artifact_export_with_read_budget<T, F>(
    runtime: &ToolRuntime,
    read_budget: &mut Duration,
    future: F,
) -> Result<T, McpArtifactExportReadError>
where
    F: Future<Output = Result<T, McpArtifactExportReadError>>,
{
    if read_budget.is_zero() {
        return Err(McpArtifactExportReadError::Timeout);
    }
    let started = Instant::now();
    let outcome = tokio::time::timeout(*read_budget, future).await;
    *read_budget = read_budget.saturating_sub(started.elapsed());
    match outcome {
        Ok(result) => result,
        Err(_) => {
            runtime
                .runner_registry
                .cancel_abandoned_sync_requests()
                .await;
            Err(McpArtifactExportReadError::Timeout)
        }
    }
}

pub(super) async fn mcp_artifact_export_stream_plan_with_gate_timeout(
    runtime: &ToolRuntime,
    uri: &str,
    auth: Option<&AuthContext>,
    gate: Arc<Semaphore>,
    admission_timeout: Duration,
    read_timeout: Duration,
) -> Result<McpArtifactExportStreamPlan, McpArtifactExportReadError> {
    let record = mcp_artifact_export_lookup(uri, auth)?;
    if auth.is_some_and(|auth| !auth.has_scope(crate::auth::SCOPE_PROJECT_READ)) {
        return Err(McpArtifactExportReadError::Forbidden {
            required_scope: Some(crate::auth::SCOPE_PROJECT_READ),
            description: format!(
                "missing required scope: {}",
                crate::auth::SCOPE_PROJECT_READ
            ),
        });
    }
    let permit = match tokio::time::timeout(admission_timeout, gate.acquire_owned()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) | Err(_) => return Err(McpArtifactExportReadError::Busy),
    };
    let mut read_budget = read_timeout;
    let snapshot = mcp_artifact_export_with_read_budget(
        runtime,
        &mut read_budget,
        mcp_artifact_export_metadata_recheck(runtime, &record, auth),
    )
    .await?;
    let max_chunks = MAX_PROJECT_ARTIFACT_EXPORT_BYTES
        .div_ceil(MAX_READ_PROJECT_ARTIFACT_LENGTH)
        .saturating_add(1);
    let mut first_chunk = Vec::new();
    let mut offset = 0usize;
    let mut chunks = 0usize;
    if snapshot.bytes > 0 {
        let length = snapshot.bytes.min(MAX_READ_PROJECT_ARTIFACT_LENGTH);
        let chunk = mcp_artifact_export_with_read_budget(
            runtime,
            &mut read_budget,
            mcp_artifact_export_read_chunk(runtime, &record, auth, 0, length),
        )
        .await?;
        offset = chunk.len();
        if offset == 0 || offset > snapshot.bytes {
            return Err(McpArtifactExportReadError::Unsafe);
        }
        chunks = 1;
        first_chunk = chunk;
    }
    Ok(McpArtifactExportStreamPlan {
        uri: uri.to_string(),
        record,
        first_chunk,
        offset,
        chunks,
        max_chunks,
        read_budget,
        _permit: permit,
    })
}

pub(super) async fn mcp_artifact_export_stream_plan(
    runtime: &ToolRuntime,
    uri: &str,
    auth: Option<&AuthContext>,
) -> Result<McpArtifactExportStreamPlan, McpArtifactExportReadError> {
    mcp_artifact_export_stream_plan_with_gate_timeout(
        runtime,
        uri,
        auth,
        mcp_artifact_export_read_semaphore(),
        MCP_ARTIFACT_EXPORT_ADMISSION_TIMEOUT,
        MCP_ARTIFACT_EXPORT_READ_TIMEOUT,
    )
    .await
}

#[derive(Default)]
pub(super) struct McpArtifactExportBase64Encoder {
    pub(super) carry: [u8; 3],
    pub(super) carry_len: usize,
}

impl McpArtifactExportBase64Encoder {
    pub(super) fn push(&mut self, bytes: &[u8]) -> String {
        let mut output = String::with_capacity(((bytes.len() + 2) / 3) * 4 + 4);
        let mut index = 0usize;
        if self.carry_len > 0 {
            let needed = 3 - self.carry_len;
            let take = needed.min(bytes.len());
            self.carry[self.carry_len..self.carry_len + take].copy_from_slice(&bytes[..take]);
            self.carry_len += take;
            index += take;
            if self.carry_len == 3 {
                general_purpose::STANDARD.encode_string(&self.carry, &mut output);
                self.carry_len = 0;
            }
        }
        let remaining = &bytes[index..];
        let aligned_len = (remaining.len() / 3) * 3;
        if aligned_len > 0 {
            general_purpose::STANDARD.encode_string(&remaining[..aligned_len], &mut output);
        }
        let tail = &remaining[aligned_len..];
        if !tail.is_empty() {
            self.carry[..tail.len()].copy_from_slice(tail);
            self.carry_len = tail.len();
        }
        output
    }

    pub(super) fn finish(&mut self) -> String {
        if self.carry_len == 0 {
            return String::new();
        }
        let output = general_purpose::STANDARD.encode(&self.carry[..self.carry_len]);
        self.carry_len = 0;
        output
    }
}

pub(super) type McpArtifactExportStreamFrame = Result<Vec<u8>, std::io::Error>;

pub(super) fn mcp_artifact_export_stream_prefix(
    id: &Value,
    uri: &str,
    mime_type: &str,
) -> Result<Vec<u8>, McpArtifactExportReadError> {
    let mut output = Vec::with_capacity(256);
    output.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":");
    output.extend_from_slice(
        &serde_json::to_vec(id).map_err(|_| McpArtifactExportReadError::Unsafe)?,
    );
    output.extend_from_slice(b",\"result\":{\"contents\":[{\"uri\":");
    output.extend_from_slice(
        &serde_json::to_vec(uri).map_err(|_| McpArtifactExportReadError::Unsafe)?,
    );
    output.extend_from_slice(b",\"mimeType\":");
    output.extend_from_slice(
        &serde_json::to_vec(mime_type).map_err(|_| McpArtifactExportReadError::Unsafe)?,
    );
    output.extend_from_slice(b",\"blob\":\"");
    Ok(output)
}

pub(super) fn mcp_artifact_export_stream_suffix() -> Result<Vec<u8>, McpArtifactExportReadError> {
    let mut output = b"\"}],\"resultType\":\"complete\",\"_meta\":{\"io.modelcontextprotocol/serverInfo\":{\"name\":\"webcodex\",\"version\":".to_vec();
    output.extend_from_slice(
        &serde_json::to_vec(env!("CARGO_PKG_VERSION"))
            .map_err(|_| McpArtifactExportReadError::Unsafe)?,
    );
    output.extend_from_slice(b"}}}}");
    Ok(output)
}

pub(super) async fn mcp_artifact_export_send_frame(
    sender: &mpsc::Sender<McpArtifactExportStreamFrame>,
    frame: Vec<u8>,
) -> Result<(), McpArtifactExportReadError> {
    sender
        .send(Ok(frame))
        .await
        .map_err(|_| McpArtifactExportReadError::Unavailable)
}

pub(super) async fn mcp_artifact_export_emit_chunk(
    sender: &mpsc::Sender<McpArtifactExportStreamFrame>,
    encoder: &mut McpArtifactExportBase64Encoder,
    sha256: &mut Sha256,
    emitted_bytes: &mut usize,
    expected_bytes: usize,
    chunk: &[u8],
) -> Result<(), McpArtifactExportReadError> {
    *emitted_bytes = emitted_bytes
        .checked_add(chunk.len())
        .ok_or(McpArtifactExportReadError::Unsafe)?;
    if *emitted_bytes > expected_bytes {
        return Err(McpArtifactExportReadError::Unsafe);
    }
    sha256.update(chunk);
    let encoded = encoder.push(chunk);
    if !encoded.is_empty() {
        mcp_artifact_export_send_frame(sender, encoded.into_bytes()).await?;
    }
    Ok(())
}

pub(super) async fn mcp_artifact_export_stream_transfer(
    runtime: &ToolRuntime,
    id: &Value,
    auth: Option<&AuthContext>,
    mut plan: McpArtifactExportStreamPlan,
    sender: mpsc::Sender<McpArtifactExportStreamFrame>,
) -> Result<(), McpArtifactExportReadError> {
    let snapshot = plan.record.snapshot.clone();
    mcp_artifact_export_send_frame(
        &sender,
        mcp_artifact_export_stream_prefix(id, &plan.uri, &snapshot.mime_type)?,
    )
    .await?;

    let mut encoder = McpArtifactExportBase64Encoder::default();
    let mut sha256 = Sha256::new();
    let mut emitted_bytes = 0usize;
    if !plan.first_chunk.is_empty() {
        let first_chunk = std::mem::take(&mut plan.first_chunk);
        mcp_artifact_export_emit_chunk(
            &sender,
            &mut encoder,
            &mut sha256,
            &mut emitted_bytes,
            snapshot.bytes,
            &first_chunk,
        )
        .await?;
    }
    if emitted_bytes != plan.offset {
        return Err(McpArtifactExportReadError::Unsafe);
    }

    while plan.offset < snapshot.bytes {
        let mut batch = Vec::with_capacity(MAX_MCP_ARTIFACT_EXPORT_CHUNK_READS);
        let mut batch_offset = plan.offset;
        while batch.len() < MAX_MCP_ARTIFACT_EXPORT_CHUNK_READS && batch_offset < snapshot.bytes {
            if plan.chunks >= plan.max_chunks {
                return Err(McpArtifactExportReadError::Unsafe);
            }
            plan.chunks = plan.chunks.saturating_add(1);
            let length = (snapshot.bytes - batch_offset).min(MAX_READ_PROJECT_ARTIFACT_LENGTH);
            batch.push((batch_offset, length));
            batch_offset = batch_offset
                .checked_add(length)
                .ok_or(McpArtifactExportReadError::Unsafe)?;
        }
        let runtime_ref = runtime;
        let record = &plan.record;
        let results = mcp_artifact_export_with_read_budget(runtime, &mut plan.read_budget, async {
            Ok(
                join_all(batch.iter().map(|&(batch_offset, length)| async move {
                    mcp_artifact_export_read_chunk(runtime_ref, record, auth, batch_offset, length)
                        .await
                }))
                .await,
            )
        })
        .await?;

        // Drain the full bounded batch before surfacing an offset-ordered error.
        // This preserves the existing no-abandoned-request rule.
        for ((requested_offset, _), result) in batch.into_iter().zip(results) {
            if requested_offset != plan.offset {
                return Err(McpArtifactExportReadError::Unsafe);
            }
            let chunk = result?;
            plan.offset = plan
                .offset
                .checked_add(chunk.len())
                .ok_or(McpArtifactExportReadError::Unsafe)?;
            mcp_artifact_export_emit_chunk(
                &sender,
                &mut encoder,
                &mut sha256,
                &mut emitted_bytes,
                snapshot.bytes,
                &chunk,
            )
            .await?;
        }
    }

    if emitted_bytes != snapshot.bytes || plan.offset != snapshot.bytes {
        return Err(McpArtifactExportReadError::Unsafe);
    }
    let final_sha256 = format!("{:x}", sha256.finalize());
    if final_sha256 != snapshot.sha256 {
        // Bytes may already have reached the HTTP peer. Fail closed by never
        // emitting the base64 tail or closing JSON suffix, so a changed file
        // cannot become a syntactically valid successful MCP resource result.
        return Err(McpArtifactExportReadError::SnapshotChanged);
    }
    let tail = encoder.finish();
    if !tail.is_empty() {
        mcp_artifact_export_send_frame(&sender, tail.into_bytes()).await?;
    }
    mcp_artifact_export_send_frame(&sender, mcp_artifact_export_stream_suffix()?).await?;
    Ok(())
}

pub(super) fn mcp_artifact_export_stream_io_error(
    error: &McpArtifactExportReadError,
) -> std::io::Error {
    let message = match error {
        McpArtifactExportReadError::SnapshotChanged => {
            "artifact export stream failed snapshot integrity validation"
        }
        McpArtifactExportReadError::Timeout => "artifact export stream timed out",
        _ => "artifact export stream failed",
    };
    std::io::Error::other(message)
}

#[cfg(test)]
pub(super) async fn mcp_artifact_export_collect_stream_response(
    runtime: &ToolRuntime,
    id: &Value,
    auth: Option<&AuthContext>,
    plan: McpArtifactExportStreamPlan,
) -> Result<Value, McpArtifactExportReadError> {
    let (sender, mut receiver) = mpsc::channel::<McpArtifactExportStreamFrame>(1);
    let transfer = mcp_artifact_export_stream_transfer(runtime, id, auth, plan, sender);
    let collect = async {
        let mut body = Vec::new();
        while let Some(frame) = receiver.recv().await {
            let frame = frame.map_err(|_| McpArtifactExportReadError::Unsafe)?;
            body.extend_from_slice(&frame);
        }
        Ok::<Vec<u8>, McpArtifactExportReadError>(body)
    };
    let (transfer_result, body_result) = tokio::join!(transfer, collect);
    transfer_result?;
    serde_json::from_slice(&body_result?).map_err(|_| McpArtifactExportReadError::Unsafe)
}

#[cfg(test)]
pub(super) async fn mcp_artifact_export_resource_read_with_gate_timeout(
    runtime: &ToolRuntime,
    uri: &str,
    auth: Option<&AuthContext>,
    gate: Arc<Semaphore>,
    admission_timeout: Duration,
    read_timeout: Duration,
) -> Result<Value, McpArtifactExportReadError> {
    let plan = mcp_artifact_export_stream_plan_with_gate_timeout(
        runtime,
        uri,
        auth,
        gate,
        admission_timeout,
        read_timeout,
    )
    .await?;
    let response =
        mcp_artifact_export_collect_stream_response(runtime, &Value::Null, auth, plan).await?;
    response
        .get("result")
        .cloned()
        .ok_or(McpArtifactExportReadError::Unsafe)
}

#[cfg(test)]
pub(super) async fn mcp_artifact_export_resource_read_with_gate(
    runtime: &ToolRuntime,
    uri: &str,
    auth: Option<&AuthContext>,
    gate: Arc<Semaphore>,
    admission_timeout: Duration,
) -> Result<Value, McpArtifactExportReadError> {
    mcp_artifact_export_resource_read_with_gate_timeout(
        runtime,
        uri,
        auth,
        gate,
        admission_timeout,
        MCP_ARTIFACT_EXPORT_READ_TIMEOUT,
    )
    .await
}

pub(super) fn mcp_artifact_export_read_error_outcome(
    id: Option<Value>,
    auth: Option<&AuthContext>,
    error: McpArtifactExportReadError,
) -> McpOutcome {
    match error {
        McpArtifactExportReadError::Forbidden {
            required_scope,
            description,
        } => scope_forbidden(auth, required_scope, description),
        McpArtifactExportReadError::Unavailable => McpOutcome::BadRequest(rpc_error(
            id,
            -32602,
            "Artifact export resource is unavailable",
        )),
        McpArtifactExportReadError::SnapshotChanged => McpOutcome::BadRequest(rpc_error(
            id,
            -32602,
            "Exported artifact no longer matches its snapshot",
        )),
        McpArtifactExportReadError::Unsafe => McpOutcome::BadRequest(rpc_error(
            id,
            -32603,
            "Artifact export resource failed bounded safety validation",
        )),
        McpArtifactExportReadError::Busy => McpOutcome::BadRequest(rpc_error(
            id,
            MCP_ARTIFACT_EXPORT_BUSY_CODE,
            "Artifact export is temporarily busy; retry later",
        )),
        McpArtifactExportReadError::Timeout => McpOutcome::BadRequest(rpc_error(
            id,
            -32603,
            "Artifact export resource read timed out",
        )),
    }
}

pub(super) fn server_capabilities(apps_enabled: bool) -> Value {
    let mut capabilities = json!({
        "tools": { "listChanged": false },
        "resources": { "listChanged": false, "subscribe": false }
    });
    if apps_enabled {
        capabilities["extensions"] = json!({
            MCP_UI_EXTENSION: {
                "mimeTypes": [MCP_UI_RESOURCE_MIME_TYPE]
            }
        });
    }
    capabilities
}

pub(super) fn mcp_app_enabled(
    server_apps_enabled: bool,
    stateless_2026: bool,
    model_surface: ModelSurface,
    params: &Value,
) -> bool {
    server_apps_enabled
        && stateless_2026
        && model_surface_supports_mcp_apps(model_surface)
        && request_supports_mcp_apps(params)
}

pub(super) fn is_artifact_export_resource_uri(uri: &str) -> bool {
    uri.starts_with(MCP_ARTIFACT_EXPORT_URI_PREFIX)
}

pub(super) fn is_snapshot_resource_uri(uri: &str) -> bool {
    uri.starts_with(MCP_SNAPSHOT_RESOURCE_URI_PREFIX)
}

pub(super) fn resource_read_bypasses_runtime_read(params: &Value) -> bool {
    params
        .get("uri")
        .and_then(Value::as_str)
        .is_some_and(|uri| is_artifact_export_resource_uri(uri) || is_snapshot_resource_uri(uri))
}

pub(super) fn handle_list(
    runtime: &ToolRuntime,
    id: Option<Value>,
    app_enabled: bool,
) -> McpOutcome {
    let result = if app_enabled {
        mcp_app_resources_list(runtime.runtime_info.configured_public_url.as_deref())
    } else {
        json!({ "resources": [] })
    };
    McpOutcome::Ok(rpc_result(id, mcp_stateless_result(result, true)))
}

pub(super) async fn handle_read(
    runtime: &ToolRuntime,
    params: Value,
    id: Option<Value>,
    auth: Option<&AuthContext>,
    model_surface: ModelSurface,
    apps_enabled: bool,
) -> McpOutcome {
    let Some(uri) = params.get("uri").and_then(Value::as_str) else {
        return McpOutcome::BadRequest(rpc_error(id, -32602, "Invalid params: uri is required"));
    };
    if is_artifact_export_resource_uri(uri) {
        let response_id = id.clone().unwrap_or(Value::Null);
        let plan = match mcp_artifact_export_stream_plan(runtime, uri, auth).await {
            Ok(plan) => plan,
            Err(error) => return mcp_artifact_export_read_error_outcome(id, auth, error),
        };
        return McpOutcome::ArtifactExportStream {
            id: response_id,
            plan,
        };
    }
    if is_snapshot_resource_uri(uri) {
        let caller = match mcp_artifact_export_caller_binding(auth) {
            Ok(caller) => caller,
            Err(_) => {
                return McpOutcome::BadRequest(rpc_error(
                    id,
                    -32602,
                    format!("Resource not found: {uri}"),
                ));
            }
        };
        let record = mcp_snapshot_resource_registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_for_caller(uri, &caller);
        let Some(record) = record else {
            return McpOutcome::BadRequest(rpc_error(
                id,
                -32602,
                format!("Resource not found: {uri}"),
            ));
        };
        for scope in match record.kind {
            McpSnapshotResourceKind::Window => &[crate::auth::SCOPE_COMPUTER_READ][..],
            McpSnapshotResourceKind::Display => &[
                crate::auth::SCOPE_COMPUTER_READ,
                crate::auth::SCOPE_COMPUTER_DISPLAY_READ,
            ][..],
        } {
            if let Some(outcome) = require_mcp_scope(auth, scope) {
                return outcome;
            }
        }
        tracing::info!(
            target: "webcodex::mcp",
            resource_kind = ?record.kind,
            file_bytes = record.bytes.len(),
            "mcp_snapshot_resource_read"
        );
        let result = json!({
            "contents": [{
                "uri": uri,
                "mimeType": record.mime_type,
                "blob": general_purpose::STANDARD.encode(record.bytes.as_ref())
            }]
        });
        return McpOutcome::Ok(rpc_result(id, mcp_stateless_result(result, true)));
    }

    // Artifact/snapshot resources above remain available independently. The
    // global switch controls only optional static MCP App presentation.
    if !apps_enabled {
        return McpOutcome::BadRequest(rpc_error(
            id,
            -32602,
            "MCP App resources are disabled by Server configuration",
        ));
    }

    // Tool descriptors advertise the App resource independently of whether a
    // later resource fetch repeats UI client-capability metadata.
    if !model_surface_supports_mcp_apps(model_surface) {
        return McpOutcome::BadRequest(rpc_error(
            id,
            -32602,
            "MCP App resource is unavailable on this model surface",
        ));
    }
    let Some(result) =
        mcp_static_app_resource_read(uri, runtime.runtime_info.configured_public_url.as_deref())
    else {
        return McpOutcome::BadRequest(rpc_error(id, -32602, format!("Resource not found: {uri}")));
    };
    let mut result = mcp_stateless_result(result, true);
    if uri == MCP_COMPUTER_UI_RESOURCE_URI {
        result["ttlMs"] = Value::from(MCP_COMPUTER_UI_RESOURCE_TTL_MS);
    }
    McpOutcome::Ok(rpc_result(id, result))
}

#[derive(Debug, Default)]
pub(super) struct McpResourceToolCallContext {
    artifact_export_caller: Option<McpArtifactExportCallerBinding>,
    snapshot_resource_caller: Option<McpArtifactExportCallerBinding>,
}

#[derive(Debug)]
pub(super) enum McpResourceToolCallPrepareError {
    UnsupportedExportSurface,
    ArtifactCallerBinding(&'static str),
}

impl McpResourceToolCallPrepareError {
    pub(super) fn message(&self) -> String {
        match self {
            Self::UnsupportedExportSurface => {
                "export_project_artifact requires a stateless-2026 operator-capable MCP surface"
                    .to_string()
            }
            Self::ArtifactCallerBinding(error) => {
                format!("export_project_artifact cannot bind this caller: {error}")
            }
        }
    }

    pub(super) fn records_model_ergonomics_failure(&self) -> bool {
        matches!(self, Self::ArtifactCallerBinding(_))
    }
}

pub(super) fn prepare_tool_call(
    tool_name: &str,
    stateless_2026: bool,
    model_surface: ModelSurface,
    auth: Option<&AuthContext>,
) -> Result<McpResourceToolCallContext, McpResourceToolCallPrepareError> {
    let artifact_export_caller = if tool_name == "export_project_artifact" {
        if !stateless_2026 || !model_surface.supports_operator_extensions() {
            return Err(McpResourceToolCallPrepareError::UnsupportedExportSurface);
        }
        Some(
            mcp_artifact_export_caller_binding(auth)
                .map_err(McpResourceToolCallPrepareError::ArtifactCallerBinding)?,
        )
    } else {
        None
    };
    let snapshot_resource_caller = if stateless_2026
        && model_surface.supports_operator_extensions()
        && matches!(tool_name, "computer_snapshot" | "computer_snapshot_display")
    {
        mcp_artifact_export_caller_binding(auth).ok()
    } else {
        None
    };
    Ok(McpResourceToolCallContext {
        artifact_export_caller,
        snapshot_resource_caller,
    })
}

pub(super) enum McpResourceToolResultAdaptation {
    Framed(Value),
    Unhandled(ToolResult),
}

pub(super) fn adapt_tool_result(
    tool_name: &str,
    as_image_requested: bool,
    result: ToolResult,
    context: McpResourceToolCallContext,
) -> McpResourceToolResultAdaptation {
    if tool_name == "export_project_artifact" {
        return McpResourceToolResultAdaptation::Framed(mcp_artifact_export_tool_result(
            result,
            context
                .artifact_export_caller
                .expect("validated artifact export caller binding"),
        ));
    }
    if matches!(tool_name, "computer_snapshot" | "computer_snapshot_display")
        || (tool_name == "read_project_artifact" && as_image_requested)
    {
        return McpResourceToolResultAdaptation::Framed(
            mcp_runtime_tool_result_with_snapshot_resource(
                tool_name,
                as_image_requested,
                result,
                context.snapshot_resource_caller,
            ),
        );
    }
    McpResourceToolResultAdaptation::Unhandled(result)
}

pub(super) fn start_artifact_export_stream(
    runtime: Arc<ToolRuntime>,
    id: Value,
    auth: Option<AuthContext>,
    plan: McpArtifactExportStreamPlan,
) -> mpsc::Receiver<McpArtifactExportStreamFrame> {
    let (sender, receiver) = mpsc::channel::<McpArtifactExportStreamFrame>(1);
    let error_sender = sender.clone();
    tokio::spawn(async move {
        let transfer =
            mcp_artifact_export_stream_transfer(&runtime, &id, auth.as_ref(), plan, sender);
        match tokio::time::timeout(MCP_ARTIFACT_EXPORT_STREAM_TIMEOUT, transfer).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = error_sender
                    .send(Err(mcp_artifact_export_stream_io_error(&error)))
                    .await;
            }
            Err(_) => {
                runtime
                    .runner_registry
                    .cancel_abandoned_sync_requests()
                    .await;
                let _ = error_sender
                    .send(Err(std::io::Error::other(
                        "artifact export stream exceeded bounded transfer timeout",
                    )))
                    .await;
            }
        }
    });
    receiver
}
