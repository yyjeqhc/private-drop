use crate::{Config, Database, MessageKind};

use super::security::is_sensitive_path;
use super::types::{
    ArtifactOperation, ArtifactPlan, ArtifactRequest, ArtifactResponse, EditOperation, EditRequest,
    EditResponse,
};
use super::url_security::{is_allowed_chatgpt_estuary_url, validate_source_url};
use std::path::Path;

const MAX_BINARY_ARTIFACT_SIZE: usize = 5 * 1024 * 1024;

pub(super) fn resolve_upload_file_id(
    config: &Config,
    db: &Database,
    file_id: &str,
    rel_path: &str,
) -> Result<String, String> {
    let message = db
        .get_message(file_id)
        .map_err(|e| format!("Failed to read upload record: {}", e))?
        .ok_or_else(|| format!("file_id not found: {}", file_id))?;
    if message.kind != MessageKind::File {
        return Err(format!("file_id is not a file upload: {}", file_id));
    }
    let Some(file_path) = message.file_path else {
        return Err(format!("file_id has no file_path: {}", file_id));
    };
    if file_path.is_empty() || file_path.contains("..") || is_sensitive_path(&file_path) {
        return Err("upload file_path is unsafe".to_string());
    }
    let uploads_dir = config.uploads_dir();
    let canonical_uploads = uploads_dir
        .canonicalize()
        .map_err(|e| format!("Failed to access uploads directory: {}", e))?;
    let candidate = uploads_dir.join(&file_path);
    let canonical = candidate
        .canonicalize()
        .map_err(|_| format!("file_id upload file not found: {}", file_id))?;
    if !canonical.starts_with(&canonical_uploads) {
        return Err("file_id resolved outside uploads directory".to_string());
    }
    let meta =
        std::fs::metadata(&canonical).map_err(|e| format!("Failed to stat upload file: {}", e))?;
    if !meta.is_file() {
        return Err("file_id upload path is not a regular file".to_string());
    }
    if meta.len() as usize > MAX_BINARY_ARTIFACT_SIZE {
        return Err(format!(
            "upload file for {} exceeds {} bytes",
            rel_path, MAX_BINARY_ARTIFACT_SIZE
        ));
    }
    Ok(canonical.to_string_lossy().to_string())
}

pub(super) fn markdown_snippet_for_artifact(path: &str, alt_text: Option<&str>) -> String {
    let alt = alt_text.unwrap_or("Generated artifact");
    let file_name = Path::new(path)
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or(path);
    format!("![{}](./{})", alt, file_name)
}

pub(super) fn companion_markdown_content(body: &ArtifactRequest, snippet: &str) -> Option<String> {
    body.companion_markdown_path.as_ref()?;
    if let Some(template) = &body.companion_markdown_template {
        return Some(
            template
                .replace("{{path}}", &body.path)
                .replace("{{markdown_snippet}}", snippet)
                .replace(
                    "{{alt_text}}",
                    body.alt_text.as_deref().unwrap_or("Generated artifact"),
                ),
        );
    }
    Some(format!(
        "# Generated Artifact\n\nThis companion file was created by `saveProjectArtifact`.\n\n- Artifact path: `{}`\n- Source workflow: `save_generated` / artifact ingest\n\n{}\n",
        body.path, snippet
    ))
}

pub(super) fn base64_decoded_len(base64_content: &str) -> Option<u64> {
    let trimmed = base64_content.trim_end_matches('=');
    let padding = base64_content.len().saturating_sub(trimmed.len());
    Some(((base64_content.len() * 3) / 4).saturating_sub(padding) as u64)
}

pub(super) fn artifact_edit_from_base64(
    body: &ArtifactRequest,
    base64_content: String,
) -> EditOperation {
    if body.allow_overwrite {
        EditOperation::WriteBinaryArtifact {
            path: body.path.clone(),
            base64_content,
            allow_overwrite: true,
        }
    } else {
        EditOperation::CreateBinaryArtifact {
            path: body.path.clone(),
            base64_content,
        }
    }
}

pub(super) fn artifact_edit_from_upload(
    body: &ArtifactRequest,
    source_file: String,
) -> EditOperation {
    if body.allow_overwrite {
        EditOperation::WriteBinaryFileFromUpload {
            path: body.path.clone(),
            source_file,
            allow_overwrite: true,
        }
    } else {
        EditOperation::CreateBinaryFileFromUpload {
            path: body.path.clone(),
            source_file,
        }
    }
}

pub(super) fn artifact_edit_from_url(body: &ArtifactRequest, source_url: String) -> EditOperation {
    if body.allow_overwrite {
        EditOperation::WriteBinaryFileFromUrl {
            path: body.path.clone(),
            source_url,
            allow_overwrite: true,
        }
    } else {
        EditOperation::CreateBinaryFileFromUrl {
            path: body.path.clone(),
            source_url,
        }
    }
}

pub(super) fn provided_artifact_sources(body: &ArtifactRequest) -> Vec<&'static str> {
    let mut sources = Vec::new();
    if body.file_id.is_some() {
        sources.push("file_id");
    }
    if body.base64_content.is_some() {
        sources.push("base64_content");
    }
    if body.chatgpt_estuary_url.is_some() {
        sources.push("chatgpt_estuary_url");
    }
    if body.source_url.is_some() {
        sources.push("source_url");
    }
    if body.source_file.is_some() {
        sources.push("source_file");
    }
    sources
}

pub(super) fn multiple_source_warning(
    body: &ArtifactRequest,
    selected_source: &str,
) -> Vec<String> {
    if matches!(body.op, ArtifactOperation::SaveGenerated) {
        let sources = provided_artifact_sources(body);
        if sources.len() > 1 {
            return vec![format!(
                "Multiple artifact sources provided; using {} by priority.",
                selected_source
            )];
        }
    }
    Vec::new()
}

pub(super) fn select_generated_artifact_edit(
    body: &ArtifactRequest,
    config: &Config,
    db: &Database,
) -> Result<(EditOperation, Option<u64>, &'static str), String> {
    if let Some(file_id) = body.file_id.as_deref() {
        let source_file = resolve_upload_file_id(config, db, file_id, &body.path)?;
        return Ok((
            artifact_edit_from_upload(body, source_file),
            None,
            "file_id",
        ));
    }
    if let Some(base64_content) = body.base64_content.clone() {
        let file_size = base64_decoded_len(&base64_content);
        return Ok((
            artifact_edit_from_base64(body, base64_content),
            file_size,
            "base64_content",
        ));
    }
    if let Some(source_url) = body.chatgpt_estuary_url.clone() {
        validate_source_url(&source_url)?;
        let parsed = reqwest::Url::parse(&source_url)
            .map_err(|e| format!("Invalid chatgpt_estuary_url: {}", e))?;
        if !is_allowed_chatgpt_estuary_url(&parsed) {
            return Err(
                "chatgpt_estuary_url is not an allowed ChatGPT estuary content URL".to_string(),
            );
        }
        return Ok((
            artifact_edit_from_url(body, source_url),
            None,
            "chatgpt_estuary_url",
        ));
    }
    if let Some(source_url) = body.source_url.clone() {
        return Ok((artifact_edit_from_url(body, source_url), None, "source_url"));
    }
    if let Some(source_file) = body.source_file.clone() {
        return Ok((
            artifact_edit_from_upload(body, source_file),
            None,
            "source_file",
        ));
    }
    Err("save_generated requires one of file_id, base64_content, chatgpt_estuary_url, source_url, or source_file".to_string())
}

pub(super) fn plan_artifact_request(
    body: &ArtifactRequest,
    config: &Config,
    db: &Database,
) -> Result<ArtifactPlan, String> {
    let (artifact_edit, file_size, selected_source) = match body.op {
        ArtifactOperation::SaveBase64 => {
            let base64_content = body
                .base64_content
                .clone()
                .ok_or_else(|| "base64_content is required for save_base64".to_string())?;
            let file_size = base64_decoded_len(&base64_content);
            (
                artifact_edit_from_base64(body, base64_content),
                file_size,
                "base64_content",
            )
        }
        ArtifactOperation::SaveUpload => {
            let source_file = if let Some(file_id) = body.file_id.as_deref() {
                resolve_upload_file_id(config, db, file_id, &body.path)?
            } else {
                body.source_file.clone().ok_or_else(|| {
                    "file_id or source_file is required for save_upload".to_string()
                })?
            };
            let selected_source = if body.file_id.is_some() {
                "file_id"
            } else {
                "source_file"
            };
            (
                artifact_edit_from_upload(body, source_file),
                None,
                selected_source,
            )
        }
        ArtifactOperation::SaveUrl => {
            let source_url = body
                .source_url
                .clone()
                .or_else(|| body.chatgpt_estuary_url.clone())
                .ok_or_else(|| "source_url is required for save_url".to_string())?;
            let selected_source = if body.source_url.is_some() {
                "source_url"
            } else {
                "chatgpt_estuary_url"
            };
            (
                artifact_edit_from_url(body, source_url),
                None,
                selected_source,
            )
        }
        ArtifactOperation::SaveGenerated => select_generated_artifact_edit(body, config, db)?,
    };
    let snippet = markdown_snippet_for_artifact(&body.path, body.alt_text.as_deref());
    let mut edits = vec![artifact_edit];
    if let Some(content) = companion_markdown_content(body, &snippet) {
        if let Some(path) = &body.companion_markdown_path {
            if body.allow_overwrite {
                edits.push(EditOperation::WriteFile {
                    path: path.clone(),
                    content,
                    allow_overwrite: true,
                });
            } else {
                edits.push(EditOperation::CreateFile {
                    path: path.clone(),
                    content,
                });
            }
        }
    }
    Ok(ArtifactPlan {
        edit_request: EditRequest {
            project: body.project.clone(),
            reason: body.reason.clone(),
            dry_run: false,
            edits,
        },
        saved_path: body.path.clone(),
        relative_path: body.path.clone(),
        file_size,
        mime_type: body.mime_type.clone(),
        markdown_snippet: Some(snippet),
        selected_source: selected_source.to_string(),
        warnings: multiple_source_warning(body, selected_source),
    })
}

pub(super) fn artifact_response_from_edit(
    plan: &ArtifactPlan,
    response: EditResponse,
) -> ArtifactResponse {
    let mut warnings = plan.warnings.clone();
    warnings.extend(response.warnings);
    if response.success {
        ArtifactResponse {
            success: true,
            changed_files: response.changed_files,
            saved_path: Some(plan.saved_path.clone()),
            relative_path: Some(plan.relative_path.clone()),
            file_size: plan.file_size,
            mime_type: plan.mime_type.clone(),
            markdown_snippet: plan.markdown_snippet.clone(),
            selected_source: Some(plan.selected_source.clone()),
            diff: response.diff,
            warnings,
            error: None,
        }
    } else {
        ArtifactResponse {
            success: false,
            changed_files: response.changed_files,
            saved_path: None,
            relative_path: None,
            file_size: None,
            mime_type: plan.mime_type.clone(),
            markdown_snippet: plan.markdown_snippet.clone(),
            selected_source: Some(plan.selected_source.clone()),
            diff: response.diff,
            warnings,
            error: response.error,
        }
    }
}
