use super::*;

pub(crate) const MAX_PROJECT_ARTIFACT_BYTES: usize = 10 * 1024 * 1024; // 10 MiB

/// Maximum final size for one streaming artifact export.
pub(crate) const MAX_PROJECT_ARTIFACT_EXPORT_BYTES: usize = 256 * 1024 * 1024; // 256 MiB

/// Maximum final size for one chunked artifact upload.
pub(crate) const MAX_PROJECT_ARTIFACT_UPLOAD_BYTES: usize = 256 * 1024 * 1024; // 256 MiB

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectArtifactExportSnapshot {
    pub(crate) path: String,
    pub(crate) bytes: usize,
    pub(crate) sha256: String,
    pub(crate) mime_type: String,
    pub(crate) name: String,
}

/// Default returned segment size for `read_project_artifact`. This tool returns
/// base64 content in the JSON response, so keep chunks small for GPT Actions.
pub(crate) const DEFAULT_READ_PROJECT_ARTIFACT_LENGTH: usize = 32 * 1024; // 32 KiB

/// Maximum returned segment size for `read_project_artifact`.
pub(crate) const MAX_READ_PROJECT_ARTIFACT_LENGTH: usize = 64 * 1024; // 64 KiB

/// Maximum decoded size accepted for one `artifact_upload_chunk` request.
pub(crate) const MAX_PROJECT_ARTIFACT_UPLOAD_CHUNK_BYTES: usize = 1024 * 1024; // 1 MiB

/// Hard cap for a base64-encoded artifact payload plus JSON overhead.
pub(crate) const MAX_PROJECT_ARTIFACT_BASE64_BYTES: usize = 14 * 1024 * 1024; // ~10 MiB decoded

/// Exact maximum standard-base64 length for one upload chunk. Transport JSON
/// has separate bounded headroom and does not expand this decoded data limit.
pub(crate) const MAX_PROJECT_ARTIFACT_UPLOAD_CHUNK_BASE64_BYTES: usize =
    ((MAX_PROJECT_ARTIFACT_UPLOAD_CHUNK_BYTES + 2) / 3) * 4;

fn sniff_supported_mcp_image_mime(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(b"\xff\xd8") {
        Some("image/jpeg")
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn validate_mcp_image_artifact_output(output: &Value) -> Result<(), String> {
    let mime_type = output
        .get("mime_type")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "MCP image read requires a detected image/png, image/jpeg, or image/webp MIME type"
                .to_string()
        })?;
    if !matches!(mime_type, "image/png" | "image/jpeg" | "image/webp") {
        return Err(format!(
            "unsupported MCP image MIME type '{mime_type}'; supported types are image/png, image/jpeg, and image/webp"
        ));
    }
    let encoded = output
        .get("content_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| "MCP image read did not return complete base64 content".to_string())?;
    let decoded = general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("MCP image read returned invalid base64: {error}"))?;
    if decoded.len() > MAX_MCP_IMAGE_BYTES {
        return Err(format!(
            "MCP image is too large; maximum is {} bytes",
            MAX_MCP_IMAGE_BYTES
        ));
    }
    let detected = sniff_supported_mcp_image_mime(&decoded).ok_or_else(|| {
        "artifact content is not a supported PNG, JPEG, or WebP image".to_string()
    })?;
    if detected != mime_type {
        return Err(format!(
            "artifact MIME mismatch: runner reported '{mime_type}' but content is '{detected}'"
        ));
    }
    let file_bytes = output
        .get("file_bytes")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| "MCP image read did not report file_bytes".to_string())?;
    let bytes_returned = output
        .get("bytes_returned")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| "MCP image read did not report bytes_returned".to_string())?;
    let complete = output.get("offset").and_then(Value::as_u64) == Some(0)
        && output.get("truncated").and_then(Value::as_bool) == Some(false)
        && output.get("eof").and_then(Value::as_bool) == Some(true)
        && file_bytes == decoded.len()
        && bytes_returned == decoded.len();
    if !complete {
        return Err("MCP image read returned an incomplete artifact".to_string());
    }
    let reported_sha256 = output
        .get("sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| "MCP image read did not report sha256".to_string())?;
    if sha256_hex_bytes(&decoded) != reported_sha256 {
        return Err("MCP image read returned content that does not match its sha256".to_string());
    }
    Ok(())
}

/// Validate a project-relative binary artifact path. This is stricter than
/// source edit validation: in addition to build/VCS dirs it rejects secrets,
/// token paths, and private-key filenames.
pub(crate) fn validate_artifact_file_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("path cannot be empty".to_string());
    }
    if path.contains('\0') {
        return Err("path cannot contain NUL bytes".to_string());
    }
    let p = Path::new(path);
    let bytes = path.as_bytes();
    let windows_drive_prefix =
        bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if p.has_root() || path.starts_with('\\') || windows_drive_prefix {
        return Err("path must be project-relative".to_string());
    }
    if path.split(['/', '\\']).any(|component| component == "..") {
        return Err("path cannot contain parent traversal".to_string());
    }
    if is_sensitive_artifact_path(path) {
        return Err(format!("refusing sensitive artifact path '{}'", path));
    }
    Ok(())
}

pub(crate) fn is_sensitive_artifact_path(path: &str) -> bool {
    // Artifacts previously missed `*.key` and Runner configs; they now share the
    // same policy as edits.
    crate::sensitive_paths::is_bulk_skipped_path(path)
}

fn validate_artifact_mime(mime_type: Option<&str>) -> Result<Option<String>, String> {
    let Some(mime) = mime_type.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if ooxml_extension_for_mime(mime).is_some() {
        return Ok(Some(mime.to_string()));
    }
    match mime {
        "image/png"
        | "image/jpeg"
        | "image/webp"
        | "audio/mpeg"
        | "video/mp4"
        | "application/pdf"
        | "application/zip"
        | "text/plain"
        | "text/csv"
        | "application/json" => Ok(Some(mime.to_string())),
        "application/octet-stream" => Ok(Some(mime.to_string())),
        _ => Err(format!("unsupported mime_type '{}'; allowed artifact MIME types are image/png, image/jpeg, image/webp, audio/mpeg, video/mp4, application/pdf, application/zip, application/vnd.openxmlformats-officedocument.wordprocessingml.document, application/vnd.openxmlformats-officedocument.presentationml.presentation, application/vnd.openxmlformats-officedocument.spreadsheetml.sheet, text/plain, text/csv, application/json", mime)),
    }
}

pub(crate) fn validate_artifact_mime_for_path(
    path: &str,
    mime_type: Option<&str>,
) -> Result<Option<String>, String> {
    let mime_type = validate_artifact_mime(mime_type)?;
    if matches!(mime_type.as_deref(), Some("application/octet-stream"))
        && !has_safe_octet_stream_artifact_extension(path)
    {
        return Err(octet_stream_safe_extension_error());
    }
    if let Some(mime) = mime_type.as_deref() {
        if let Some(required_extension) = ooxml_extension_for_mime(mime) {
            if !path.to_ascii_lowercase().ends_with(required_extension) {
                return Err(format!(
                    "OOXML MIME type '{mime}' requires a matching {required_extension} artifact path"
                ));
            }
        }
    }
    Ok(mime_type)
}

fn artifact_policy_rejected_result(path: &str, message: String) -> ToolResult {
    ToolResult::err_with_output(
        message.clone(),
        json!({
            "path": path,
            "error": message,
            "failure_kind": "policy_rejected",
            "error_kind": "policy_rejected",
        }),
    )
}

fn validate_artifact_upload_id(upload_id: &str) -> Result<(), String> {
    if !upload_id.starts_with("wc_upload_") {
        return Err("upload_id must start with wc_upload_".to_string());
    }
    if upload_id.len() > 96 {
        return Err("upload_id too long".to_string());
    }
    if !upload_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("upload_id contains unsupported characters".to_string());
    }
    Ok(())
}

pub(crate) fn validate_project_artifact_export_snapshot(
    path: &str,
    output: &Value,
) -> Result<ProjectArtifactExportSnapshot, String> {
    validate_artifact_file_path(path)?;
    if output.get("path").and_then(Value::as_str) != Some(path) {
        return Err("artifact metadata path does not match the requested export path".to_string());
    }
    let bytes = output
        .get("bytes")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| "artifact metadata did not report a valid byte count".to_string())?;
    if bytes > MAX_PROJECT_ARTIFACT_EXPORT_BYTES {
        return Err(format!(
            "artifact is too large to export; maximum is {} bytes",
            MAX_PROJECT_ARTIFACT_EXPORT_BYTES
        ));
    }
    let sha256 = output
        .get("sha256")
        .and_then(Value::as_str)
        .filter(|value| is_hex_sha256(value))
        .ok_or_else(|| "artifact metadata did not report a valid sha256".to_string())?
        .to_string();
    let reported_mime = output
        .get("mime_type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "artifact export requires a detected or inferred MIME type".to_string())?;
    let mime_type = validate_artifact_mime_for_path(path, Some(reported_mime))?
        .ok_or_else(|| "artifact export requires a validated MIME type".to_string())?;
    let name = Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty() && *value != "." && *value != "..")
        .ok_or_else(|| "artifact export path does not have a safe basename".to_string())?;
    if name.len() > 255
        || name
            .chars()
            .any(|ch| ch.is_control() || ch == '/' || ch == '\\')
    {
        return Err("artifact export basename is not safe for MCP presentation".to_string());
    }
    Ok(ProjectArtifactExportSnapshot {
        path: path.to_string(),
        bytes,
        sha256,
        mime_type,
        name: name.to_string(),
    })
}

impl ToolRuntime {
    /// Internal-only large-file metadata transport for MCP artifact export.
    /// The Runner registry atomically rechecks the generation-2 streaming
    /// metadata and chunk-read baseline while admitting the request.
    async fn run_runner_json_artifact_export_metadata_op(
        &self,
        client_id: String,
        cwd: String,
        path: String,
        payload: Value,
        auth: Option<&AuthContext>,
    ) -> Result<Value, String> {
        let serialized = serde_json::to_string(&payload).map_err(|error| {
            format!("failed to serialize artifact export metadata payload: {error}")
        })?;
        let wait_timeout = 60_u64;
        let request = ShellFileOpRequest {
            op: "read_project_artifact_metadata".to_string(),
            client_id,
            path: path.clone(),
            cwd: Some(cwd),
            content: Some(serialized),
            max_bytes: None,
            old_text: None,
            pattern: None,
            expected_sha256: None,
            expected_prefix: None,
            start_line: None,
            end_line: None,
            line: None,
            create_dirs: false,
            wait_timeout_secs: wait_timeout,
        };
        let (request_id, rx) = self
            .runner_registry
            .enqueue_artifact_export_metadata(
                request,
                "mcp_artifact_export".to_string(),
                crate::runner_http::runner_access_from_auth(auth).as_ref(),
            )
            .await?;
        let response = match tokio::time::timeout(Duration::from_secs(wait_timeout + 4), rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => {
                self.runner_registry.cancel_request(&request_id).await;
                return Err("agent export_project_artifact request was dropped".to_string());
            }
            Err(_) => {
                self.runner_registry.cancel_request(&request_id).await;
                return Err("timed out waiting for agent export_project_artifact".to_string());
            }
        };
        if let Some(error) = response.error {
            return Err(error);
        }
        if response.exit_code != Some(0) {
            return Err(response.stderr.unwrap_or_else(|| {
                format!(
                    "agent export_project_artifact failed with code {:?}",
                    response.exit_code
                )
            }));
        }
        let stdout = response.stdout.unwrap_or_default();
        let stdout = stdout.trim();
        let output = serde_json::from_str(stdout).map_err(|error| {
            format!(
                "agent export_project_artifact returned invalid JSON: {error} (got: {})",
                &stdout[..stdout.len().min(200)]
            )
        })?;
        Ok(output)
    }

    /// Internal-only segment transport for MCP artifact export. This is not a
    /// ToolCall and has no model schema. Project ownership and Runner access are
    /// re-resolved for the authenticated caller before each enqueue; the registry
    /// then atomically rechecks file_read plus the generation-2 chunk-read baseline.
    pub(crate) async fn read_project_artifact_export_chunk_internal(
        &self,
        project: &str,
        path: &str,
        expected_file_bytes: usize,
        offset: usize,
        length: usize,
        auth: Option<&AuthContext>,
    ) -> Result<Value, String> {
        if let Err(error) = validate_artifact_file_path(path) {
            return Err(error);
        }
        if expected_file_bytes > MAX_PROJECT_ARTIFACT_EXPORT_BYTES {
            return Err(format!(
                "artifact is too large to export; maximum is {} bytes",
                MAX_PROJECT_ARTIFACT_EXPORT_BYTES
            ));
        }
        if length == 0 || length > MAX_READ_PROJECT_ARTIFACT_LENGTH {
            return Err(format!(
                "artifact export chunk length must be between 1 and {} bytes",
                MAX_READ_PROJECT_ARTIFACT_LENGTH
            ));
        }
        offset
            .checked_add(length)
            .ok_or_else(|| "artifact export offset + length overflow".to_string())?;
        let resolved = self
            .resolve_project_for_auth(project, auth)
            .await
            .map_err(|error| error.to_message())?;
        let client_id = resolved.client_id.clone();
        let payload = json!({
            "path": path,
            "expected_file_bytes": expected_file_bytes,
            "offset": offset,
            "length": length,
        });
        let serialized = serde_json::to_string(&payload).map_err(|error| {
            format!("failed to serialize artifact export chunk payload: {error}")
        })?;
        let wait_timeout = 60_u64;
        let request = ShellFileOpRequest {
            op: "read_project_artifact_export_chunk".to_string(),
            client_id,
            path: path.to_string(),
            cwd: Some(resolved.path.clone()),
            content: Some(serialized),
            max_bytes: None,
            old_text: None,
            pattern: None,
            expected_sha256: None,
            expected_prefix: None,
            start_line: None,
            end_line: None,
            line: None,
            create_dirs: false,
            wait_timeout_secs: wait_timeout,
        };
        let (request_id, rx) = self
            .runner_registry
            .enqueue_artifact_export_chunk(
                request,
                "mcp_artifact_export".to_string(),
                crate::runner_http::runner_access_from_auth(auth).as_ref(),
            )
            .await?;
        let response = match tokio::time::timeout(Duration::from_secs(wait_timeout + 4), rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => {
                self.runner_registry.cancel_request(&request_id).await;
                return Err("agent artifact export chunk request was dropped".to_string());
            }
            Err(_) => {
                self.runner_registry.cancel_request(&request_id).await;
                return Err("timed out waiting for agent artifact export chunk".to_string());
            }
        };
        if let Some(error) = response.error {
            return Err(error);
        }
        if response.exit_code != Some(0) {
            return Err(response.stderr.unwrap_or_else(|| {
                format!(
                    "agent artifact export chunk failed with code {:?}",
                    response.exit_code
                )
            }));
        }
        let stdout = response.stdout.unwrap_or_default();
        let stdout = stdout.trim();
        let output = serde_json::from_str(stdout).map_err(|error| {
            format!(
                "agent artifact export chunk returned invalid JSON: {error} (got: {})",
                &stdout[..stdout.len().min(200)]
            )
        })?;
        Ok(output)
    }

    pub(crate) async fn save_project_artifact(
        &self,
        project: String,
        path: String,
        content_base64: String,
        mime_type: Option<String>,
        overwrite: Option<bool>,
    ) -> ToolResult {
        if let Err(e) = validate_artifact_file_path(&path) {
            return artifact_policy_rejected_result(&path, e);
        }
        if content_base64.len() > MAX_PROJECT_ARTIFACT_BASE64_BYTES {
            return ToolResult::err(format!(
                "content_base64 too large; maximum encoded size is {} bytes",
                MAX_PROJECT_ARTIFACT_BASE64_BYTES
            ));
        }
        let mime_type = match validate_artifact_mime_for_path(&path, mime_type.as_deref()) {
            Ok(v) => v,
            Err(e) => return artifact_policy_rejected_result(&path, e),
        };
        let decoded = match general_purpose::STANDARD.decode(content_base64.as_bytes()) {
            Ok(bytes) => bytes,
            Err(e) => return ToolResult::err(format!("invalid base64: {}", e)),
        };
        if decoded.len() > MAX_PROJECT_ARTIFACT_BYTES {
            return ToolResult::err(format!(
                "decoded artifact too large; maximum is {} bytes",
                MAX_PROJECT_ARTIFACT_BYTES
            ));
        }
        let proj = match self.resolve_project(&project).await {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        let client_id = proj.client_id.clone();

        let payload = json!({
            "path": path.clone(),
            "content_base64": content_base64,
            "mime_type": mime_type,
            "overwrite": overwrite.unwrap_or(false),
            "max_bytes": MAX_PROJECT_ARTIFACT_BYTES,
        });
        let obj = match self
            .run_runner_json_file_op(
                client_id,
                proj.path.clone(),
                path.clone(),
                "save_project_artifact",
                payload,
                "save_project_artifact",
            )
            .await
        {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };
        if let Some(err) = obj
            .get("error")
            .and_then(|e| e.as_str())
            .map(str::to_string)
        {
            return ToolResult {
                success: false,
                output: obj,
                error: Some(err),
            };
        }
        ToolResult::ok(obj)
    }

    pub(crate) async fn read_project_artifact_metadata(
        &self,
        project: String,
        path: String,
        allow_missing: Option<bool>,
    ) -> ToolResult {
        if let Err(e) = validate_artifact_file_path(&path) {
            return artifact_policy_rejected_result(&path, e);
        }
        let proj = match self.resolve_project(&project).await {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        let client_id = proj.client_id.clone();
        let payload = json!({
            "path": path.clone(),
            "max_bytes": MAX_PROJECT_ARTIFACT_BYTES,
            "allow_missing": allow_missing.unwrap_or(false),
        });
        let obj = match self
            .run_runner_json_file_op(
                client_id,
                proj.path.clone(),
                path.clone(),
                "read_project_artifact_metadata",
                payload,
                "read_project_artifact_metadata",
            )
            .await
        {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };
        if let Some(err) = obj
            .get("error")
            .and_then(|e| e.as_str())
            .map(str::to_string)
        {
            return ToolResult {
                success: false,
                output: obj,
                error: Some(err),
            };
        }
        ToolResult::ok(obj)
    }

    pub(crate) async fn export_project_artifact_metadata_resolved(
        &self,
        resolved: &ResolvedProject,
        path: String,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        if let Err(error) = validate_artifact_file_path(&path) {
            return artifact_policy_rejected_result(&path, error);
        }
        let client_id = resolved.config.client_id.clone();
        let streaming_payload = json!({
            "path": path.clone(),
            "max_bytes": MAX_PROJECT_ARTIFACT_EXPORT_BYTES,
            "allow_missing": false,
        });
        let output = match self
            .run_runner_json_artifact_export_metadata_op(
                client_id,
                resolved.config.path.clone(),
                path.clone(),
                streaming_payload,
                auth,
            )
            .await
        {
            Ok(output) => output,
            Err(error) => return ToolResult::err(error),
        };
        if let Some(error) = output
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            return ToolResult {
                success: false,
                output,
                error: Some(error),
            };
        }
        let snapshot = match validate_project_artifact_export_snapshot(&path, &output) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return ToolResult::err_with_output(
                    error,
                    json!({
                        "project": resolved.resolved_id,
                        "path": path,
                        "error_kind": "invalid_artifact_export_metadata",
                    }),
                )
            }
        };
        ToolResult::ok(json!({
            "project": resolved.resolved_id,
            "path": snapshot.path,
            "bytes": snapshot.bytes,
            "sha256": snapshot.sha256,
            "mime_type": snapshot.mime_type,
            "name": snapshot.name,
        }))
    }

    pub(crate) async fn read_project_artifact_export_metadata_internal(
        &self,
        project: &str,
        path: &str,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        if let Err(error) = validate_artifact_file_path(path) {
            return artifact_policy_rejected_result(path, error);
        }
        let resolved = match self.resolve_project_input_for_auth(project, auth).await {
            Ok(resolved) => resolved,
            Err(error) => return error.into_tool_result(),
        };
        self.export_project_artifact_metadata_resolved(&resolved, path.to_string(), auth)
            .await
    }

    pub(crate) async fn read_project_artifact(
        &self,
        project: String,
        path: String,
        encoding: Option<String>,
        offset: Option<usize>,
        length: Option<usize>,
        as_image: Option<bool>,
    ) -> ToolResult {
        if let Err(e) = validate_artifact_file_path(&path) {
            return artifact_policy_rejected_result(&path, e);
        }
        let encoding = encoding.unwrap_or_else(|| "base64".to_string());
        if encoding != "base64" {
            return ToolResult::err("unsupported encoding; only 'base64' is currently supported");
        }
        let as_image = as_image.unwrap_or(false);
        if as_image && (offset.is_some() || length.is_some()) {
            return ToolResult::err(
                "as_image cannot be combined with offset or length; the MCP image path always reads one complete bounded image",
            );
        }
        let offset = offset.unwrap_or(0);
        let length = if as_image {
            MAX_MCP_IMAGE_BYTES
        } else {
            length.unwrap_or(DEFAULT_READ_PROJECT_ARTIFACT_LENGTH)
        };
        if length == 0 {
            return ToolResult::err("length must be at least 1");
        }
        if !as_image && length > MAX_READ_PROJECT_ARTIFACT_LENGTH {
            return ToolResult::err(format!(
                "length too large; maximum is {} bytes",
                MAX_READ_PROJECT_ARTIFACT_LENGTH
            ));
        }
        let proj = match self.resolve_project(&project).await {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        let client_id = proj.client_id.clone();
        let mut payload = json!({
            "path": path.clone(),
            "offset": offset,
            "length": length,
            "max_file_bytes": if as_image {
                MAX_MCP_IMAGE_BYTES
            } else {
                MAX_PROJECT_ARTIFACT_BYTES
            },
        });
        if as_image {
            payload["mcp_image"] = json!(true);
        }
        let obj = match self
            .run_runner_json_file_op(
                client_id,
                proj.path.clone(),
                path.clone(),
                "read_project_artifact",
                payload,
                "read_project_artifact",
            )
            .await
        {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };
        if let Some(err) = obj
            .get("error")
            .and_then(|e| e.as_str())
            .map(str::to_string)
        {
            return ToolResult {
                success: false,
                output: obj,
                error: Some(err),
            };
        }
        if as_image {
            if let Err(error) = validate_mcp_image_artifact_output(&obj) {
                return ToolResult::err_with_output(
                    error,
                    json!({
                        "path": path,
                        "error_kind": "invalid_mcp_image_artifact",
                    }),
                );
            }
        }
        ToolResult::ok(obj)
    }

    async fn run_project_artifact_write_file_op(
        &self,
        project: String,
        path: String,
        payload: Value,
        op: &str,
        tool_name: &str,
    ) -> ToolResult {
        let proj = match self.resolve_project(&project).await {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        let client_id = proj.client_id.clone();
        let obj = match self
            .run_runner_json_file_op(client_id, proj.path.clone(), path, op, payload, tool_name)
            .await
        {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };
        if let Some(err) = obj
            .get("error")
            .and_then(|e| e.as_str())
            .map(str::to_string)
        {
            return ToolResult {
                success: false,
                output: obj,
                error: Some(err),
            };
        }
        ToolResult::ok(obj)
    }

    pub(crate) async fn artifact_upload_begin(
        &self,
        project: String,
        path: String,
        expected_bytes: Option<usize>,
        expected_sha256: Option<String>,
        mime_type: Option<String>,
        overwrite: Option<bool>,
    ) -> ToolResult {
        if let Err(e) = validate_artifact_file_path(&path) {
            return artifact_policy_rejected_result(&path, e);
        }
        if let Some(bytes) = expected_bytes {
            if bytes > MAX_PROJECT_ARTIFACT_UPLOAD_BYTES {
                return ToolResult::err(format!(
                    "expected_bytes too large; maximum is {} bytes",
                    MAX_PROJECT_ARTIFACT_UPLOAD_BYTES
                ));
            }
        }
        if let Some(hash) = expected_sha256.as_deref() {
            if !is_hex_sha256(hash) {
                return ToolResult::err(
                    "expected_sha256 must be a lowercase 64-char hex sha256 digest".to_string(),
                );
            }
        }
        let mime_type = match validate_artifact_mime_for_path(&path, mime_type.as_deref()) {
            Ok(v) => v,
            Err(e) => return artifact_policy_rejected_result(&path, e),
        };
        let payload = json!({
            "path": path.clone(),
            "expected_bytes": expected_bytes,
            "expected_sha256": expected_sha256,
            "mime_type": mime_type,
            "overwrite": overwrite.unwrap_or(false),
            "max_bytes": MAX_PROJECT_ARTIFACT_UPLOAD_BYTES,
        });
        self.run_project_artifact_write_file_op(
            project,
            path,
            payload,
            "artifact_upload_begin",
            "artifact_upload_begin",
        )
        .await
    }

    pub(crate) async fn artifact_upload_chunk(
        &self,
        project: String,
        path: String,
        upload_id: String,
        offset: usize,
        content_base64: String,
    ) -> ToolResult {
        if let Err(e) = validate_artifact_file_path(&path) {
            return artifact_policy_rejected_result(&path, e);
        }
        if let Err(e) = validate_artifact_upload_id(&upload_id) {
            return ToolResult::err(e);
        }
        if content_base64.len() > MAX_PROJECT_ARTIFACT_UPLOAD_CHUNK_BASE64_BYTES {
            return ToolResult::err(format!(
                "content_base64 chunk too large; maximum encoded size is {} bytes",
                MAX_PROJECT_ARTIFACT_UPLOAD_CHUNK_BASE64_BYTES
            ));
        }
        let decoded = match general_purpose::STANDARD.decode(content_base64.as_bytes()) {
            Ok(bytes) => bytes,
            Err(e) => return ToolResult::err(format!("invalid base64: {}", e)),
        };
        if decoded.is_empty() {
            return ToolResult::err("decoded chunk must contain at least 1 byte");
        }
        if decoded.len() > MAX_PROJECT_ARTIFACT_UPLOAD_CHUNK_BYTES {
            return ToolResult::err(format!(
                "decoded chunk too large; maximum is {} bytes",
                MAX_PROJECT_ARTIFACT_UPLOAD_CHUNK_BYTES
            ));
        }
        let payload = json!({
            "path": path.clone(),
            "upload_id": upload_id,
            "offset": offset,
            "content_base64": content_base64,
            "max_chunk_bytes": MAX_PROJECT_ARTIFACT_UPLOAD_CHUNK_BYTES,
        });
        self.run_project_artifact_write_file_op(
            project,
            path,
            payload,
            "artifact_upload_chunk",
            "artifact_upload_chunk",
        )
        .await
    }

    pub(crate) async fn artifact_upload_finish(
        &self,
        project: String,
        path: String,
        upload_id: String,
    ) -> ToolResult {
        if let Err(e) = validate_artifact_file_path(&path) {
            return artifact_policy_rejected_result(&path, e);
        }
        if let Err(e) = validate_artifact_upload_id(&upload_id) {
            return ToolResult::err(e);
        }
        let payload = json!({
            "path": path.clone(),
            "upload_id": upload_id,
        });
        self.run_project_artifact_write_file_op(
            project,
            path,
            payload,
            "artifact_upload_finish",
            "artifact_upload_finish",
        )
        .await
    }

    pub(crate) async fn artifact_upload_abort(
        &self,
        project: String,
        path: String,
        upload_id: String,
    ) -> ToolResult {
        if let Err(e) = validate_artifact_file_path(&path) {
            return artifact_policy_rejected_result(&path, e);
        }
        if let Err(e) = validate_artifact_upload_id(&upload_id) {
            return ToolResult::err(e);
        }
        let payload = json!({
            "path": path.clone(),
            "upload_id": upload_id,
        });
        self.run_project_artifact_write_file_op(
            project,
            path,
            payload,
            "artifact_upload_abort",
            "artifact_upload_abort",
        )
        .await
    }
}
