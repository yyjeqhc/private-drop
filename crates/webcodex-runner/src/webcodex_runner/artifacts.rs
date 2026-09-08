use super::files::sha256_hex_bytes;
use super::output::{line_edit_stdout, CommandResult};
use crate::apply_edits_shared::is_lowercase_hex_sha256 as is_hex_sha256;
use crate::artifact_policy::{
    has_safe_octet_stream_artifact_extension, octet_stream_safe_extension_error, DOCX_MIME,
    MAX_MCP_IMAGE_BYTES, PPTX_MIME, XLSX_MIME,
};
#[cfg(test)]
use crate::runner_protocol::RunnerRequest;
use base64::{engine::general_purpose, Engine as _};
use flate2::read::DeflateDecoder;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
#[cfg(test)]
use webcodex_core::runner_operation::RunnerOperation;
use webcodex_core::runner_operation::{RunnerFileOperation, RunnerFilePayload};
use xml::reader::{EventReader, XmlEvent};

const DEFAULT_MAX_ARTIFACT_BYTES: usize = 10 * 1024 * 1024;
const MAX_ARTIFACT_EXPORT_BYTES: usize = 256 * 1024 * 1024;
const MAX_ARTIFACT_UPLOAD_BYTES: usize = 256 * 1024 * 1024;
const DEFAULT_ARTIFACT_READ_LENGTH: usize = 32 * 1024;
const MAX_ARTIFACT_EXPORT_CHUNK_BYTES: usize = 64 * 1024;
const MAX_ARTIFACT_UPLOAD_CHUNK_BYTES: usize = 1024 * 1024;
const ARTIFACT_UPLOAD_IDLE_TTL_SECS: u64 = 24 * 60 * 60;
const MAX_ACTIVE_ARTIFACT_UPLOADS_PER_PROJECT: usize = 32;
const MAX_ARTIFACT_UPLOAD_RESERVED_BYTES_PER_PROJECT: usize = 512 * 1024 * 1024;
const MAX_ARTIFACT_UPLOAD_PROJECT_SCAN_ENTRIES: usize = 100_000;
const MAX_ARTIFACT_UPLOAD_STATE_BYTES: usize = 4 * 1024;
static ARTIFACT_UPLOAD_STATE_LOCK: Mutex<()> = Mutex::new(());
const ARTIFACT_STREAM_BUFFER_BYTES: usize = 64 * 1024;
const ZIP_EOCD_MAX_SEARCH_BYTES: usize = 65_557;
const MAX_OOXML_ZIP_ENTRIES: usize = 4096;
const MAX_OOXML_CENTRAL_DIRECTORY_BYTES: usize = 2 * 1024 * 1024;
const MAX_OOXML_CONTENT_TYPES_BYTES: usize = 256 * 1024;
const MAX_OOXML_CONTENT_TYPE_EVENTS: usize = 4096;
const OOXML_CONTENT_TYPES_NAMESPACE: &str =
    "http://schemas.openxmlformats.org/package/2006/content-types";

#[cfg(test)]
pub(crate) fn is_artifact_request_kind(kind: &str) -> bool {
    matches!(
        kind,
        "file_save_project_artifact"
            | "file_read_project_artifact_metadata"
            | "file_read_project_artifact"
            | "file_read_project_artifact_export_chunk"
            | "file_artifact_upload_begin"
            | "file_artifact_upload_chunk"
            | "file_artifact_upload_finish"
            | "file_artifact_upload_abort"
    )
}

pub(crate) fn validate_artifact_runner_path(path: &str) -> Result<(), String> {
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

fn is_sensitive_artifact_path(path: &str) -> bool {
    webcodex_core::sensitive_paths::is_bulk_skipped_path(path)
}

fn parse_json_payload(request: &RunnerFilePayload) -> Result<Value, String> {
    let Some(content) = request.content.as_deref() else {
        return Err("invalid json: missing file-op payload".to_string());
    };
    serde_json::from_str(content).map_err(|e| format!("invalid json: {}", e))
}

fn parse_bool_field(payload: &Value, key: &str) -> Result<bool, String> {
    match payload.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("{key} must be a boolean")),
    }
}

fn parse_usize_field(payload: &Value, key: &str, default: usize) -> Result<usize, String> {
    match payload.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(number)) => number
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| format!("{key} must be an integer")),
        Some(Value::String(value)) => value
            .parse::<usize>()
            .map_err(|_| format!("{key} must be an integer")),
        Some(_) => Err(format!("{key} must be an integer")),
    }
}

fn parse_optional_usize_field(payload: &Value, key: &str) -> Result<Option<usize>, String> {
    match payload.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| format!("{key} must be an integer")),
        Some(Value::String(value)) => value
            .parse::<usize>()
            .map(Some)
            .map_err(|_| format!("{key} must be an integer")),
        Some(_) => Err(format!("{key} must be an integer")),
    }
}

fn parse_optional_clean_string(
    payload: &Value,
    key: &str,
    max_len: usize,
) -> Result<Option<String>, String> {
    match payload.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.len() <= max_len && !value.contains('\0') => {
            Ok(Some(value.clone()))
        }
        Some(Value::String(_)) => Err(format!("{key} is invalid")),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn parse_required_clean_string(
    payload: &Value,
    key: &str,
    max_len: usize,
) -> Result<String, String> {
    match payload.get(key) {
        Some(Value::String(value)) if value.len() <= max_len && !value.contains('\0') => {
            Ok(value.clone())
        }
        Some(Value::String(_)) => Err(format!("{key} is invalid")),
        Some(_) => Err(format!("{key} must be a string")),
        None => Err(format!("{key} is required")),
    }
}

fn validate_upload_id(upload_id: &str) -> Result<(), String> {
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

fn project_root(request: &RunnerFilePayload) -> Result<std::path::PathBuf, String> {
    let Some(cwd) = request.cwd.as_deref() else {
        return Err("artifact request missing project root".to_string());
    };
    std::fs::canonicalize(cwd).map_err(|e| format!("project root does not exist: {}", e))
}

fn ensure_existing_target_in_project_root(resolved: &Path, root: &Path) -> Result<(), String> {
    let target = std::fs::canonicalize(resolved).map_err(|e| format!("read failed: {}", e))?;
    if target != root && !target.starts_with(root) {
        return Err("artifact path escapes project root".to_string());
    }
    Ok(())
}

fn ensure_parent_in_project_root(resolved: &Path, root: &Path) -> Result<(), String> {
    let parent = resolved
        .parent()
        .ok_or_else(|| "target path has no parent directory".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| format!("write failed: {}", e))?;
    let parent = std::fs::canonicalize(parent).map_err(|e| format!("write failed: {}", e))?;
    if parent != root && !parent.starts_with(root) {
        return Err("artifact path escapes project root".to_string());
    }
    Ok(())
}

fn ensure_existing_parent_in_project_root(resolved: &Path, root: &Path) -> Result<(), String> {
    let parent = resolved
        .parent()
        .ok_or_else(|| "target path has no parent directory".to_string())?;
    let parent = std::fs::canonicalize(parent).map_err(|e| format!("upload failed: {}", e))?;
    if parent != root && !parent.starts_with(root) {
        return Err("artifact path escapes project root".to_string());
    }
    Ok(())
}

fn write_bytes_atomic_strict(path: &Path, data: &[u8], overwrite: bool) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "target path has no parent directory".to_string())?;
    let mut last_error = None;
    for attempt in 0..16 {
        let tmp = parent.join(format!(".wc-artifact-{}-{}", std::process::id(), attempt));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(data) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e.to_string());
                }
                if let Err(e) = file.sync_all() {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e.to_string());
                }
                drop(file);
                if overwrite {
                    if let Err(e) = std::fs::rename(&tmp, path) {
                        let _ = std::fs::remove_file(&tmp);
                        return Err(e.to_string());
                    }
                } else {
                    match std::fs::hard_link(&tmp, path) {
                        Ok(()) => {
                            // The target link is now committed atomically without replacing an
                            // existing path. Cleanup failure may leave only the private temp link;
                            // it cannot invalidate or change the committed target.
                            let _ = std::fs::remove_file(&tmp);
                        }
                        Err(e) => {
                            let _ = std::fs::remove_file(&tmp);
                            if e.kind() == std::io::ErrorKind::AlreadyExists {
                                return Err("file exists and overwrite is false".to_string());
                            }
                            return Err(e.to_string());
                        }
                    }
                }
                if let Ok(dir) = std::fs::File::open(parent) {
                    let _ = dir.sync_all();
                }
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(e.to_string());
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Err(last_error.unwrap_or_else(|| "could not create temporary artifact file".to_string()))
}

fn commit_artifact_upload_part(part: &Path, target: &Path, overwrite: bool) -> Result<(), String> {
    if overwrite {
        return std::fs::rename(part, target).map_err(|e| format!("upload finish failed: {e}"));
    }
    match std::fs::hard_link(part, target) {
        Ok(()) => {
            // The final target now exists without replacing any concurrent writer.
            // If private-part cleanup fails, the target remains authoritative and a
            // later upload sweep can remove the orphaned private link safely.
            let _ = std::fs::remove_file(part);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err("file exists and overwrite is false".to_string())
        }
        Err(e) => Err(format!("upload finish failed: {e}")),
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ArtifactUploadState {
    path: String,
    expected_bytes: Option<usize>,
    expected_sha256: Option<String>,
    mime_type: Option<String>,
    overwrite: bool,
    max_bytes: usize,
}

fn upload_paths(parent: &Path, upload_id: &str) -> (PathBuf, PathBuf) {
    (
        parent.join(format!(".wc-upload-{upload_id}.part")),
        parent.join(format!(".wc-upload-{upload_id}.json")),
    )
}

fn new_upload_id(attempt: usize) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("wc_upload_{}_{}_{}", std::process::id(), nanos, attempt)
}

fn write_upload_state(sidecar: &Path, state: &ArtifactUploadState) -> Result<(), String> {
    let data = serde_json::to_vec(state).map_err(|e| format!("upload state failed: {e}"))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(sidecar)
        .map_err(|e| format!("upload state failed: {e}"))?;
    file.write_all(&data)
        .map_err(|e| format!("upload state failed: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("upload state failed: {e}"))?;
    Ok(())
}

fn read_upload_state_file(sidecar: &Path) -> Result<ArtifactUploadState, String> {
    let file = File::open(sidecar).map_err(|e| format!("upload not found: {e}"))?;
    let mut reader = file.take((MAX_ARTIFACT_UPLOAD_STATE_BYTES + 1) as u64);
    let mut data = Vec::new();
    reader
        .read_to_end(&mut data)
        .map_err(|e| format!("invalid upload state: {e}"))?;
    if data.len() > MAX_ARTIFACT_UPLOAD_STATE_BYTES {
        return Err("invalid upload state: sidecar too large".to_string());
    }
    serde_json::from_slice(&data).map_err(|e| format!("invalid upload state: {e}"))
}

fn read_upload_state(sidecar: &Path, requested_path: &str) -> Result<ArtifactUploadState, String> {
    let state = read_upload_state_file(sidecar)?;
    if state.path != requested_path {
        return Err("upload_id does not belong to requested path".to_string());
    }
    Ok(state)
}

#[derive(Default)]
struct ArtifactUploadTempFiles {
    part: Option<PathBuf>,
    sidecar: Option<PathBuf>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ArtifactUploadProjectUsage {
    active_uploads: usize,
    reserved_bytes: usize,
}

fn upload_temp_id(name: &str, suffix: &str) -> Option<String> {
    let upload_id = name.strip_prefix(".wc-upload-")?.strip_suffix(suffix)?;
    validate_upload_id(upload_id).ok()?;
    Some(upload_id.to_string())
}

fn remove_upload_temp_file(path: &Path) -> Result<bool, String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("upload cleanup failed: {e}")),
    }
}

fn upload_pair_is_stale(
    part_metadata: &std::fs::Metadata,
    sidecar_metadata: &std::fs::Metadata,
    now: SystemTime,
    idle_ttl: Duration,
) -> bool {
    let newest_modified = match (part_metadata.modified(), sidecar_metadata.modified()) {
        (Ok(part), Ok(sidecar)) => Some(if part >= sidecar { part } else { sidecar }),
        (Ok(part), Err(_)) => Some(part),
        (Err(_), Ok(sidecar)) => Some(sidecar),
        (Err(_), Err(_)) => None,
    };
    newest_modified
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age >= idle_ttl)
}

fn upload_scan_skips_directory(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        ".git" | "target" | "node_modules" | "secrets" | "tokens"
    )
}

fn sweep_artifact_upload_project(
    root: &Path,
    now: SystemTime,
    idle_ttl: Duration,
) -> Result<ArtifactUploadProjectUsage, String> {
    let mut usage = ArtifactUploadProjectUsage::default();
    let mut directories = vec![root.to_path_buf()];
    let mut scanned_entries = 0usize;

    while let Some(directory) = directories.pop() {
        let entries =
            std::fs::read_dir(&directory).map_err(|e| format!("upload cleanup failed: {e}"))?;
        let mut uploads: BTreeMap<String, ArtifactUploadTempFiles> = BTreeMap::new();
        for entry in entries {
            scanned_entries = scanned_entries
                .checked_add(1)
                .ok_or_else(|| "artifact upload project scan count overflow".to_string())?;
            if scanned_entries > MAX_ARTIFACT_UPLOAD_PROJECT_SCAN_ENTRIES {
                return Err(format!(
                    "artifact upload project has more than {MAX_ARTIFACT_UPLOAD_PROJECT_SCAN_ENTRIES} entries; refusing unbounded resource scan"
                ));
            }
            let entry = entry.map_err(|e| format!("upload cleanup failed: {e}"))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if let Some(upload_id) = upload_temp_id(&name, ".part") {
                uploads.entry(upload_id).or_default().part = Some(entry.path());
                continue;
            }
            if let Some(upload_id) = upload_temp_id(&name, ".json") {
                uploads.entry(upload_id).or_default().sidecar = Some(entry.path());
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|e| format!("upload cleanup failed: {e}"))?;
            if file_type.is_dir() && !upload_scan_skips_directory(&name) {
                directories.push(entry.path());
            }
        }

        let mut cleaned = false;
        for files in uploads.into_values() {
            match (files.part, files.sidecar) {
                (Some(part), Some(sidecar)) => {
                    let part_metadata = match std::fs::metadata(&part) {
                        Ok(metadata) => metadata,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            cleaned |= remove_upload_temp_file(&sidecar)?;
                            continue;
                        }
                        Err(e) => return Err(format!("upload cleanup failed: {e}")),
                    };
                    let sidecar_metadata = match std::fs::metadata(&sidecar) {
                        Ok(metadata) => metadata,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            cleaned |= remove_upload_temp_file(&part)?;
                            continue;
                        }
                        Err(e) => return Err(format!("upload cleanup failed: {e}")),
                    };
                    if upload_pair_is_stale(&part_metadata, &sidecar_metadata, now, idle_ttl) {
                        cleaned |= remove_upload_temp_file(&part)?;
                        cleaned |= remove_upload_temp_file(&sidecar)?;
                        continue;
                    }
                    let state = match read_upload_state_file(&sidecar) {
                        Ok(state) => state,
                        Err(_) => {
                            cleaned |= remove_upload_temp_file(&part)?;
                            cleaned |= remove_upload_temp_file(&sidecar)?;
                            continue;
                        }
                    };
                    usage.reserved_bytes = usage
                        .reserved_bytes
                        .checked_add(state.max_bytes)
                        .ok_or_else(|| {
                            "artifact upload reserved byte count overflow".to_string()
                        })?;
                    usage.active_uploads = usage
                        .active_uploads
                        .checked_add(1)
                        .ok_or_else(|| "artifact upload count overflow".to_string())?;
                }
                (Some(part), None) => cleaned |= remove_upload_temp_file(&part)?,
                (None, Some(sidecar)) => cleaned |= remove_upload_temp_file(&sidecar)?,
                (None, None) => {}
            }
        }
        if cleaned {
            if let Ok(dir) = std::fs::File::open(&directory) {
                let _ = dir.sync_all();
            }
        }
    }

    Ok(usage)
}

fn current_artifact_upload_project_usage(
    root: &Path,
) -> Result<ArtifactUploadProjectUsage, String> {
    sweep_artifact_upload_project(
        root,
        SystemTime::now(),
        Duration::from_secs(ARTIFACT_UPLOAD_IDLE_TTL_SECS),
    )
}

fn enforce_artifact_upload_begin_admission(
    usage: &ArtifactUploadProjectUsage,
    requested_max_bytes: usize,
) -> Result<(), String> {
    if usage.active_uploads >= MAX_ACTIVE_ARTIFACT_UPLOADS_PER_PROJECT {
        return Err(format!(
            "artifact upload project reached active upload limit ({MAX_ACTIVE_ARTIFACT_UPLOADS_PER_PROJECT})"
        ));
    }
    let next_reserved_bytes = usage
        .reserved_bytes
        .checked_add(requested_max_bytes)
        .ok_or_else(|| "artifact upload reserved byte count overflow".to_string())?;
    if next_reserved_bytes > MAX_ARTIFACT_UPLOAD_RESERVED_BYTES_PER_PROJECT {
        return Err(format!(
            "artifact upload project reserved byte quota exceeded ({MAX_ARTIFACT_UPLOAD_RESERVED_BYTES_PER_PROJECT})"
        ));
    }
    Ok(())
}

fn save_error(path: Option<&str>, msg: impl Into<String>) -> Value {
    json!({
        "path": path,
        "bytes_written": 0,
        "sha256": Value::Null,
        "mime_type": Value::Null,
        "error": msg.into(),
    })
}

fn upload_error(path: Option<&str>, upload_id: Option<&str>, msg: impl Into<String>) -> Value {
    json!({
        "path": path,
        "upload_id": upload_id,
        "received_bytes": 0,
        "expected_bytes": Value::Null,
        "expected_sha256": Value::Null,
        "sha256": Value::Null,
        "mime_type": Value::Null,
        "committed": false,
        "aborted": false,
        "error": msg.into(),
    })
}

fn upload_policy_rejected_error(
    path: Option<&str>,
    upload_id: Option<&str>,
    msg: impl Into<String>,
) -> Value {
    let mut out = upload_error(path, upload_id, msg);
    out["failure_kind"] = json!("policy_rejected");
    out["error_kind"] = json!("policy_rejected");
    out
}

fn metadata_error(path: Option<&str>, msg: impl Into<String>) -> Value {
    json!({
        "path": path,
        "bytes": 0,
        "sha256": Value::Null,
        "mime_type": Value::Null,
        "error": msg.into(),
    })
}

fn read_error(path: Option<&str>, msg: impl Into<String>) -> Value {
    json!({
        "path": path,
        "mime_type": Value::Null,
        "file_bytes": 0,
        "sha256": Value::Null,
        "offset": 0,
        "bytes_returned": 0,
        "content_base64": "",
        "next_offset": 0,
        "truncated": false,
        "eof": false,
        "error": msg.into(),
    })
}

fn magic_mime(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(b"\xff\xd8") {
        Some("image/jpeg")
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else if data.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if data.starts_with(b"PK\x03\x04") || data.starts_with(b"PK\x05\x06") {
        Some("application/zip")
    } else {
        None
    }
}

#[derive(Clone, Copy)]
struct ZipEntryMetadata {
    flags: u16,
    compression_method: u16,
    compressed_size: usize,
    uncompressed_size: usize,
    local_header_offset: usize,
}

fn le_u16(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        data.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn le_u32(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn zip_eocd_offset(data: &[u8]) -> Option<usize> {
    const EOCD_LEN: usize = 22;
    if data.len() < EOCD_LEN {
        return None;
    }
    let search_start = data.len().saturating_sub(65_557);
    for offset in (search_start..=data.len() - EOCD_LEN).rev() {
        if data.get(offset..offset + 4)? != b"PK\x05\x06" {
            continue;
        }
        let comment_len = usize::from(le_u16(data, offset + 20)?);
        if offset.checked_add(EOCD_LEN)?.checked_add(comment_len)? == data.len() {
            return Some(offset);
        }
    }
    None
}

fn validated_zip_entry_payload<'a>(
    data: &'a [u8],
    central_directory_offset: usize,
    entry: ZipEntryMetadata,
    expected_name: &[u8],
) -> Option<&'a [u8]> {
    if entry.flags & 0x0001 != 0 || !matches!(entry.compression_method, 0 | 8) {
        return None;
    }
    let local = entry.local_header_offset;
    if data.get(local..local.checked_add(4)?)? != b"PK\x03\x04"
        || le_u16(data, local + 6)? != entry.flags
        || le_u16(data, local + 8)? != entry.compression_method
    {
        return None;
    }
    let local_compressed_size = usize::try_from(le_u32(data, local + 18)?).ok()?;
    let local_uncompressed_size = usize::try_from(le_u32(data, local + 22)?).ok()?;
    if entry.flags & 0x0008 == 0 {
        if local_compressed_size != entry.compressed_size
            || local_uncompressed_size != entry.uncompressed_size
        {
            return None;
        }
    } else if (local_compressed_size != 0 && local_compressed_size != entry.compressed_size)
        || (local_uncompressed_size != 0 && local_uncompressed_size != entry.uncompressed_size)
    {
        return None;
    }
    let name_len = usize::from(le_u16(data, local + 26)?);
    let extra_len = usize::from(le_u16(data, local + 28)?);
    let name_start = local.checked_add(30)?;
    let name_end = name_start.checked_add(name_len)?;
    if data.get(name_start..name_end)? != expected_name {
        return None;
    }
    let compressed_start = name_end.checked_add(extra_len)?;
    let compressed_end = compressed_start.checked_add(entry.compressed_size)?;
    if compressed_end > central_directory_offset {
        return None;
    }
    data.get(compressed_start..compressed_end)
}

fn read_ooxml_content_types_entry(
    data: &[u8],
    central_directory_offset: usize,
    entry: ZipEntryMetadata,
) -> Option<Vec<u8>> {
    if entry.compressed_size > MAX_OOXML_CONTENT_TYPES_BYTES
        || entry.uncompressed_size > MAX_OOXML_CONTENT_TYPES_BYTES
    {
        return None;
    }
    let compressed = validated_zip_entry_payload(
        data,
        central_directory_offset,
        entry,
        b"[Content_Types].xml",
    )?;
    let decoded = match entry.compression_method {
        0 => {
            if entry.compressed_size != entry.uncompressed_size {
                return None;
            }
            compressed.to_vec()
        }
        8 => {
            let decoder = DeflateDecoder::new(compressed);
            let mut limited = decoder.take((MAX_OOXML_CONTENT_TYPES_BYTES + 1) as u64);
            let mut decoded = Vec::new();
            limited.read_to_end(&mut decoded).ok()?;
            if decoded.len() > MAX_OOXML_CONTENT_TYPES_BYTES {
                return None;
            }
            decoded
        }
        _ => return None,
    };
    if decoded.len() != entry.uncompressed_size {
        return None;
    }
    Some(decoded)
}

fn ooxml_content_type_mime(content_types: &[u8]) -> Option<&'static str> {
    let parser = EventReader::new(content_types);
    let mut root_seen = false;
    let mut detected = None;
    let mut event_count = 0usize;
    for event in parser {
        event_count = event_count.checked_add(1)?;
        if event_count > MAX_OOXML_CONTENT_TYPE_EVENTS {
            return None;
        }
        let event = event.ok()?;
        if let XmlEvent::StartElement {
            name, attributes, ..
        } = event
        {
            if !root_seen {
                if name.local_name != "Types"
                    || name.namespace.as_deref() != Some(OOXML_CONTENT_TYPES_NAMESPACE)
                {
                    return None;
                }
                root_seen = true;
                continue;
            }
            if name.local_name != "Override"
                || name.namespace.as_deref() != Some(OOXML_CONTENT_TYPES_NAMESPACE)
            {
                continue;
            }
            let mut part_name = None;
            let mut content_type = None;
            for attribute in attributes {
                match attribute.name.local_name.as_str() {
                    "PartName" => part_name = Some(attribute.value),
                    "ContentType" => content_type = Some(attribute.value),
                    _ => {}
                }
            }
            let candidate = match (part_name.as_deref(), content_type.as_deref()) {
                (
                    Some("/word/document.xml"),
                    Some(
                        "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
                    ),
                ) => Some(DOCX_MIME),
                (
                    Some("/ppt/presentation.xml"),
                    Some(
                        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
                    ),
                ) => Some(PPTX_MIME),
                (
                    Some("/xl/workbook.xml"),
                    Some(
                        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
                    ),
                ) => Some(XLSX_MIME),
                _ => None,
            };
            if let Some(candidate) = candidate {
                if detected.is_some_and(|mime| mime != candidate) {
                    return None;
                }
                detected = Some(candidate);
            }
        }
    }
    root_seen.then_some(detected).flatten()
}

fn ooxml_mime(data: &[u8]) -> Option<&'static str> {
    if !data.starts_with(b"PK\x03\x04") {
        return None;
    }
    let eocd = zip_eocd_offset(data)?;
    if le_u16(data, eocd + 4)? != 0 || le_u16(data, eocd + 6)? != 0 {
        return None;
    }
    let entries_on_disk = le_u16(data, eocd + 8)?;
    let total_entries = le_u16(data, eocd + 10)?;
    if entries_on_disk != total_entries || total_entries == u16::MAX {
        return None;
    }
    let entry_count = usize::from(total_entries);
    if entry_count == 0 || entry_count > MAX_OOXML_ZIP_ENTRIES {
        return None;
    }
    let central_directory_size = usize::try_from(le_u32(data, eocd + 12)?).ok()?;
    let central_directory_offset = usize::try_from(le_u32(data, eocd + 16)?).ok()?;
    if central_directory_size > MAX_OOXML_CENTRAL_DIRECTORY_BYTES
        || central_directory_size == u32::MAX as usize
        || central_directory_offset == u32::MAX as usize
    {
        return None;
    }
    let central_directory_end = central_directory_offset.checked_add(central_directory_size)?;
    if central_directory_end != eocd || central_directory_end > data.len() {
        return None;
    }

    let mut content_types_entry = None;
    let mut word_document_entry = None;
    let mut presentation_entry = None;
    let mut workbook_entry = None;
    let mut cursor = central_directory_offset;
    for _ in 0..entry_count {
        if data.get(cursor..cursor.checked_add(4)?)? != b"PK\x01\x02" {
            return None;
        }
        let flags = le_u16(data, cursor + 8)?;
        let compression_method = le_u16(data, cursor + 10)?;
        let compressed_size = usize::try_from(le_u32(data, cursor + 20)?).ok()?;
        let uncompressed_size = usize::try_from(le_u32(data, cursor + 24)?).ok()?;
        let name_len = usize::from(le_u16(data, cursor + 28)?);
        let extra_len = usize::from(le_u16(data, cursor + 30)?);
        let comment_len = usize::from(le_u16(data, cursor + 32)?);
        if le_u16(data, cursor + 34)? != 0 {
            return None;
        }
        let local_header_offset = usize::try_from(le_u32(data, cursor + 42)?).ok()?;
        if compressed_size == u32::MAX as usize
            || uncompressed_size == u32::MAX as usize
            || local_header_offset == u32::MAX as usize
        {
            return None;
        }
        let name_start = cursor.checked_add(46)?;
        let name_end = name_start.checked_add(name_len)?;
        let next = name_end.checked_add(extra_len)?.checked_add(comment_len)?;
        if next > central_directory_end {
            return None;
        }
        let entry = ZipEntryMetadata {
            flags,
            compression_method,
            compressed_size,
            uncompressed_size,
            local_header_offset,
        };
        match data.get(name_start..name_end)? {
            b"[Content_Types].xml" => {
                if content_types_entry.replace(entry).is_some() {
                    return None;
                }
            }
            b"word/document.xml" => {
                if word_document_entry.replace(entry).is_some() {
                    return None;
                }
            }
            b"ppt/presentation.xml" => {
                if presentation_entry.replace(entry).is_some() {
                    return None;
                }
            }
            b"xl/workbook.xml" => {
                if workbook_entry.replace(entry).is_some() {
                    return None;
                }
            }
            _ => {}
        }
        cursor = next;
    }
    if cursor != central_directory_end {
        return None;
    }

    let content_types =
        read_ooxml_content_types_entry(data, central_directory_offset, content_types_entry?)?;
    let (mime, main_part_entry, main_part_name) = match ooxml_content_type_mime(&content_types)? {
        DOCX_MIME => (
            DOCX_MIME,
            word_document_entry?,
            b"word/document.xml".as_slice(),
        ),
        PPTX_MIME => (
            PPTX_MIME,
            presentation_entry?,
            b"ppt/presentation.xml".as_slice(),
        ),
        XLSX_MIME => (XLSX_MIME, workbook_entry?, b"xl/workbook.xml".as_slice()),
        _ => return None,
    };
    validated_zip_entry_payload(
        data,
        central_directory_offset,
        main_part_entry,
        main_part_name,
    )?;
    Some(mime)
}

fn read_file_range(file: &mut File, offset: u64, length: usize) -> Option<Vec<u8>> {
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut data = vec![0_u8; length];
    file.read_exact(&mut data).ok()?;
    Some(data)
}

fn validated_zip_entry_payload_from_file(
    file: &mut File,
    central_directory_offset: usize,
    entry: ZipEntryMetadata,
    expected_name: &[u8],
    read_payload: bool,
) -> Option<Vec<u8>> {
    if entry.flags & 0x0001 != 0 || !matches!(entry.compression_method, 0 | 8) {
        return None;
    }
    let local = entry.local_header_offset;
    let header = read_file_range(file, u64::try_from(local).ok()?, 30)?;
    if header.get(0..4)? != b"PK\x03\x04"
        || le_u16(&header, 6)? != entry.flags
        || le_u16(&header, 8)? != entry.compression_method
    {
        return None;
    }
    let local_compressed_size = usize::try_from(le_u32(&header, 18)?).ok()?;
    let local_uncompressed_size = usize::try_from(le_u32(&header, 22)?).ok()?;
    if entry.flags & 0x0008 == 0 {
        if local_compressed_size != entry.compressed_size
            || local_uncompressed_size != entry.uncompressed_size
        {
            return None;
        }
    } else if (local_compressed_size != 0 && local_compressed_size != entry.compressed_size)
        || (local_uncompressed_size != 0 && local_uncompressed_size != entry.uncompressed_size)
    {
        return None;
    }
    let name_len = usize::from(le_u16(&header, 26)?);
    let extra_len = usize::from(le_u16(&header, 28)?);
    let name_start = local.checked_add(30)?;
    let name_end = name_start.checked_add(name_len)?;
    let compressed_start = name_end.checked_add(extra_len)?;
    let compressed_end = compressed_start.checked_add(entry.compressed_size)?;
    if compressed_end > central_directory_offset {
        return None;
    }
    let name = read_file_range(file, u64::try_from(name_start).ok()?, name_len)?;
    if name != expected_name {
        return None;
    }
    if !read_payload {
        return Some(Vec::new());
    }
    read_file_range(
        file,
        u64::try_from(compressed_start).ok()?,
        entry.compressed_size,
    )
}

fn ooxml_mime_from_file(path: &Path) -> Option<&'static str> {
    let mut file = File::open(path).ok()?;
    let file_bytes = usize::try_from(file.metadata().ok()?.len()).ok()?;
    if file_bytes < 4 || read_file_range(&mut file, 0, 4)?.as_slice() != b"PK\x03\x04" {
        return None;
    }

    let tail_len = file_bytes.min(ZIP_EOCD_MAX_SEARCH_BYTES);
    let tail_start = file_bytes.checked_sub(tail_len)?;
    let tail = read_file_range(&mut file, u64::try_from(tail_start).ok()?, tail_len)?;
    let eocd_in_tail = zip_eocd_offset(&tail)?;
    let eocd = tail_start.checked_add(eocd_in_tail)?;
    if le_u16(&tail, eocd_in_tail + 4)? != 0 || le_u16(&tail, eocd_in_tail + 6)? != 0 {
        return None;
    }
    let entries_on_disk = le_u16(&tail, eocd_in_tail + 8)?;
    let total_entries = le_u16(&tail, eocd_in_tail + 10)?;
    if entries_on_disk != total_entries || total_entries == u16::MAX {
        return None;
    }
    let entry_count = usize::from(total_entries);
    if entry_count == 0 || entry_count > MAX_OOXML_ZIP_ENTRIES {
        return None;
    }
    let central_directory_size = usize::try_from(le_u32(&tail, eocd_in_tail + 12)?).ok()?;
    let central_directory_offset = usize::try_from(le_u32(&tail, eocd_in_tail + 16)?).ok()?;
    if central_directory_size > MAX_OOXML_CENTRAL_DIRECTORY_BYTES
        || central_directory_size == u32::MAX as usize
        || central_directory_offset == u32::MAX as usize
    {
        return None;
    }
    let central_directory_end = central_directory_offset.checked_add(central_directory_size)?;
    if central_directory_end != eocd || central_directory_end > file_bytes {
        return None;
    }
    let central_directory = read_file_range(
        &mut file,
        u64::try_from(central_directory_offset).ok()?,
        central_directory_size,
    )?;

    let mut content_types_entry = None;
    let mut word_document_entry = None;
    let mut presentation_entry = None;
    let mut workbook_entry = None;
    let mut cursor = 0usize;
    for _ in 0..entry_count {
        if central_directory.get(cursor..cursor.checked_add(4)?)? != b"PK\x01\x02" {
            return None;
        }
        let flags = le_u16(&central_directory, cursor + 8)?;
        let compression_method = le_u16(&central_directory, cursor + 10)?;
        let compressed_size = usize::try_from(le_u32(&central_directory, cursor + 20)?).ok()?;
        let uncompressed_size = usize::try_from(le_u32(&central_directory, cursor + 24)?).ok()?;
        let name_len = usize::from(le_u16(&central_directory, cursor + 28)?);
        let extra_len = usize::from(le_u16(&central_directory, cursor + 30)?);
        let comment_len = usize::from(le_u16(&central_directory, cursor + 32)?);
        if le_u16(&central_directory, cursor + 34)? != 0 {
            return None;
        }
        let local_header_offset = usize::try_from(le_u32(&central_directory, cursor + 42)?).ok()?;
        if compressed_size == u32::MAX as usize
            || uncompressed_size == u32::MAX as usize
            || local_header_offset == u32::MAX as usize
        {
            return None;
        }
        let name_start = cursor.checked_add(46)?;
        let name_end = name_start.checked_add(name_len)?;
        let next = name_end.checked_add(extra_len)?.checked_add(comment_len)?;
        if next > central_directory.len() {
            return None;
        }
        let entry = ZipEntryMetadata {
            flags,
            compression_method,
            compressed_size,
            uncompressed_size,
            local_header_offset,
        };
        match central_directory.get(name_start..name_end)? {
            b"[Content_Types].xml" => {
                if content_types_entry.replace(entry).is_some() {
                    return None;
                }
            }
            b"word/document.xml" => {
                if word_document_entry.replace(entry).is_some() {
                    return None;
                }
            }
            b"ppt/presentation.xml" => {
                if presentation_entry.replace(entry).is_some() {
                    return None;
                }
            }
            b"xl/workbook.xml" => {
                if workbook_entry.replace(entry).is_some() {
                    return None;
                }
            }
            _ => {}
        }
        cursor = next;
    }
    if cursor != central_directory.len() {
        return None;
    }

    let content_types_entry = content_types_entry?;
    if content_types_entry.compressed_size > MAX_OOXML_CONTENT_TYPES_BYTES
        || content_types_entry.uncompressed_size > MAX_OOXML_CONTENT_TYPES_BYTES
    {
        return None;
    }
    let compressed = validated_zip_entry_payload_from_file(
        &mut file,
        central_directory_offset,
        content_types_entry,
        b"[Content_Types].xml",
        true,
    )?;
    let content_types = match content_types_entry.compression_method {
        0 => {
            if content_types_entry.compressed_size != content_types_entry.uncompressed_size {
                return None;
            }
            compressed
        }
        8 => {
            let decoder = DeflateDecoder::new(compressed.as_slice());
            let mut limited = decoder.take((MAX_OOXML_CONTENT_TYPES_BYTES + 1) as u64);
            let mut decoded = Vec::new();
            limited.read_to_end(&mut decoded).ok()?;
            if decoded.len() > MAX_OOXML_CONTENT_TYPES_BYTES {
                return None;
            }
            decoded
        }
        _ => return None,
    };
    if content_types.len() != content_types_entry.uncompressed_size {
        return None;
    }
    let (mime, main_part_entry, main_part_name) = match ooxml_content_type_mime(&content_types)? {
        DOCX_MIME => (
            DOCX_MIME,
            word_document_entry?,
            b"word/document.xml".as_slice(),
        ),
        PPTX_MIME => (
            PPTX_MIME,
            presentation_entry?,
            b"ppt/presentation.xml".as_slice(),
        ),
        XLSX_MIME => (XLSX_MIME, workbook_entry?, b"xl/workbook.xml".as_slice()),
        _ => return None,
    };
    validated_zip_entry_payload_from_file(
        &mut file,
        central_directory_offset,
        main_part_entry,
        main_part_name,
        false,
    )?;
    Some(mime)
}

fn extension_mime(path: &str) -> Option<&'static str> {
    let lower = path.to_lowercase();
    if lower.ends_with(".png") {
        Some("image/png")
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        Some("image/jpeg")
    } else if lower.ends_with(".webp") {
        Some("image/webp")
    } else if lower.ends_with(".pdf") {
        Some("application/pdf")
    } else if lower.ends_with(".zip") {
        Some("application/zip")
    } else if lower.ends_with(".txt") {
        Some("text/plain")
    } else if lower.ends_with(".csv") {
        Some("text/csv")
    } else if lower.ends_with(".json") {
        Some("application/json")
    } else {
        None
    }
}

fn artifact_mime(path: &str, data: &[u8], sniff_json: bool) -> Option<String> {
    if let Some(mime) = ooxml_mime(data) {
        return Some(mime.to_string());
    }
    let mut mime = extension_mime(path);
    if let Some(magic) = magic_mime(data) {
        mime = Some(magic);
    } else if sniff_json {
        let first = data.iter().copied().find(|b| !b.is_ascii_whitespace());
        if matches!(first, Some(b'{') | Some(b'[')) {
            mime = Some("application/json");
        }
    }
    mime.map(str::to_string)
}

fn artifact_mime_from_file(path: &str, file_path: &Path, sniff_json: bool) -> Option<String> {
    if let Some(mime) = ooxml_mime_from_file(file_path) {
        return Some(mime.to_string());
    }
    let mut file = File::open(file_path).ok()?;
    let prefix_len = usize::try_from(file.metadata().ok()?.len()).ok()?.min(32);
    let prefix = read_file_range(&mut file, 0, prefix_len)?;
    let mut mime = extension_mime(path);
    if let Some(magic) = magic_mime(&prefix) {
        mime = Some(magic);
    } else if sniff_json {
        let mut first = prefix
            .iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace());
        if first.is_none() {
            let mut buffer = [0_u8; ARTIFACT_STREAM_BUFFER_BYTES];
            loop {
                let read = file.read(&mut buffer).ok()?;
                if read == 0 {
                    break;
                }
                first = buffer[..read]
                    .iter()
                    .copied()
                    .find(|byte| !byte.is_ascii_whitespace());
                if first.is_some() {
                    break;
                }
            }
        }
        if matches!(first, Some(b'{') | Some(b'[')) {
            mime = Some("application/json");
        }
    }
    mime.map(str::to_string)
}

fn verify_upload_file(path: &Path, max_bytes: usize) -> Result<(usize, String), String> {
    let mut file = File::open(path).map_err(|e| format!("read failed: {e}"))?;
    let mut buffer = [0_u8; ARTIFACT_STREAM_BUFFER_BYTES];
    let mut bytes = 0usize;
    let mut sha256 = Sha256::new();
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("read failed: {e}"))?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(read)
            .ok_or_else(|| "artifact size overflow".to_string())?;
        if bytes > max_bytes {
            return Err("artifact too large to inspect".to_string());
        }
        sha256.update(&buffer[..read]);
    }
    Ok((bytes, format!("{:x}", sha256.finalize())))
}

fn png_size(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() >= 24 && data.starts_with(b"\x89PNG\r\n\x1a\n") {
        let width = u32::from_be_bytes(data[16..20].try_into().ok()?);
        let height = u32::from_be_bytes(data[20..24].try_into().ok()?);
        Some((width, height))
    } else {
        None
    }
}

fn webp_size(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() >= 30
        && data.starts_with(b"RIFF")
        && &data[8..12] == b"WEBP"
        && &data[12..16] == b"VP8X"
    {
        let width =
            1 + u32::from(data[24]) + (u32::from(data[25]) << 8) + (u32::from(data[26]) << 16);
        let height =
            1 + u32::from(data[27]) + (u32::from(data[28]) << 8) + (u32::from(data[29]) << 16);
        Some((width, height))
    } else {
        None
    }
}

fn jpeg_size(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 4 || !data.starts_with(b"\xff\xd8") {
        return None;
    }
    let mut i = 2;
    while i + 9 < data.len() {
        if data[i] != 0xff {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        i += 2;
        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) {
            let height = u16::from_be_bytes(data[i + 3..i + 5].try_into().ok()?);
            let width = u16::from_be_bytes(data[i + 5..i + 7].try_into().ok()?);
            return Some((u32::from(width), u32::from(height)));
        }
        if i + 2 > data.len() {
            break;
        }
        let segment_len = usize::from(u16::from_be_bytes(data[i..i + 2].try_into().ok()?));
        if segment_len < 2 {
            break;
        }
        i = i.saturating_add(segment_len);
    }
    None
}

fn image_size(data: &[u8]) -> Option<(u32, u32)> {
    png_size(data)
        .or_else(|| jpeg_size(data))
        .or_else(|| webp_size(data))
}

fn zip_entry_count(data: &[u8]) -> Option<u16> {
    let eocd = zip_eocd_offset(data)?;
    le_u16(data, eocd + 10)
}

fn read_limited(path: &Path, max_bytes: usize) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("read failed: {}", e))?;
    let mut limited = file.take(max_bytes.saturating_add(1) as u64);
    let mut data = Vec::new();
    limited
        .read_to_end(&mut data)
        .map_err(|e| format!("read failed: {}", e))?;
    if data.len() > max_bytes {
        return Err("artifact too large to inspect".to_string());
    }
    Ok(data)
}

pub(crate) fn handle_artifact_file_operation(
    operation: &RunnerFileOperation,
    resolved: &Path,
    start: Instant,
) -> CommandResult {
    let request = operation.payload();
    match operation {
        RunnerFileOperation::SaveProjectArtifact(_) => {
            handle_save_project_artifact(request, resolved, start)
        }
        RunnerFileOperation::ReadProjectArtifactMetadata(_) => {
            handle_read_project_artifact_metadata(request, resolved, start)
        }
        RunnerFileOperation::ReadProjectArtifact(_) => {
            handle_read_project_artifact(request, resolved, start)
        }
        RunnerFileOperation::ReadProjectArtifactExportChunk(_) => {
            handle_read_project_artifact_export_chunk(request, resolved, start)
        }
        RunnerFileOperation::ArtifactUploadBegin(_)
        | RunnerFileOperation::ArtifactUploadChunk(_)
        | RunnerFileOperation::ArtifactUploadFinish(_)
        | RunnerFileOperation::ArtifactUploadAbort(_) => {
            let _upload_guard = match ARTIFACT_UPLOAD_STATE_LOCK.lock() {
                Ok(guard) => guard,
                Err(_) => {
                    return line_edit_stdout(
                        upload_error(
                            Some(request.path.as_str()),
                            None,
                            "artifact upload state lock unavailable",
                        ),
                        start,
                    )
                }
            };
            match operation {
                RunnerFileOperation::ArtifactUploadBegin(_) => {
                    handle_artifact_upload_begin(request, resolved, start)
                }
                RunnerFileOperation::ArtifactUploadChunk(_) => {
                    handle_artifact_upload_chunk(request, resolved, start)
                }
                RunnerFileOperation::ArtifactUploadFinish(_) => {
                    handle_artifact_upload_finish(request, resolved, start)
                }
                RunnerFileOperation::ArtifactUploadAbort(_) => {
                    handle_artifact_upload_abort(request, resolved, start)
                }
                _ => unreachable!("upload operation already typed"),
            }
        }
        _ => CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some("file operation is not an artifact request".to_string()),
        },
    }
}

#[cfg(test)]
pub(crate) fn handle_artifact_file_request(
    request: &RunnerRequest,
    resolved: &Path,
    start: Instant,
) -> CommandResult {
    match request.decode_operation() {
        Ok(RunnerOperation::File(operation)) => {
            handle_artifact_file_operation(&operation, resolved, start)
        }
        _ => CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some("invalid artifact wire request".to_string()),
        },
    }
}

fn handle_save_project_artifact(
    request: &RunnerFilePayload,
    resolved: &Path,
    start: Instant,
) -> CommandResult {
    let path = request.path.as_str();
    let payload = match parse_json_payload(request) {
        Ok(payload) => payload,
        Err(e) => return line_edit_stdout(save_error(None, e), start),
    };
    if let Err(e) = validate_artifact_runner_path(path) {
        return line_edit_stdout(save_error(Some(path), e), start);
    }
    let root = match project_root(request) {
        Ok(root) => root,
        Err(e) => return line_edit_stdout(save_error(Some(path), e), start),
    };
    let content_base64 = match payload.get("content_base64").and_then(Value::as_str) {
        Some(value) if !value.contains('\0') => value,
        _ => {
            return line_edit_stdout(
                save_error(
                    Some(path),
                    "content_base64 must be a base64 string without NUL",
                ),
                start,
            )
        }
    };
    let data = match general_purpose::STANDARD.decode(content_base64.as_bytes()) {
        Ok(data) => data,
        Err(e) => {
            return line_edit_stdout(
                save_error(Some(path), format!("invalid base64: {e}")),
                start,
            )
        }
    };
    let max_bytes = match parse_usize_field(&payload, "max_bytes", DEFAULT_MAX_ARTIFACT_BYTES) {
        Ok(value) => value,
        Err(e) => return line_edit_stdout(save_error(Some(path), e), start),
    };
    if data.len() > max_bytes {
        return line_edit_stdout(save_error(Some(path), "decoded artifact too large"), start);
    }
    let overwrite = match parse_bool_field(&payload, "overwrite") {
        Ok(value) => value,
        Err(e) => return line_edit_stdout(save_error(Some(path), e), start),
    };
    let mime_type = payload
        .get("mime_type")
        .filter(|value| !value.is_null())
        .and_then(Value::as_str);

    let exists = std::fs::symlink_metadata(resolved).is_ok();
    if exists && !overwrite {
        return line_edit_stdout(
            save_error(Some(path), "file exists and overwrite is false"),
            start,
        );
    }
    if exists
        && std::fs::symlink_metadata(resolved)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
    {
        return line_edit_stdout(
            save_error(Some(path), "refusing to overwrite symlink artifact path"),
            start,
        );
    }
    if let Err(e) = ensure_parent_in_project_root(resolved, &root) {
        return line_edit_stdout(save_error(Some(path), e), start);
    }
    if std::fs::symlink_metadata(resolved)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return line_edit_stdout(
            save_error(Some(path), "refusing to overwrite symlink artifact path"),
            start,
        );
    }
    if let Err(e) = write_bytes_atomic_strict(resolved, &data, overwrite) {
        return line_edit_stdout(save_error(Some(path), format!("write failed: {e}")), start);
    }

    line_edit_stdout(
        json!({
            "path": path,
            "bytes_written": data.len(),
            "sha256": sha256_hex_bytes(&data),
            "mime_type": mime_type,
        }),
        start,
    )
}

fn handle_artifact_upload_begin(
    request: &RunnerFilePayload,
    resolved: &Path,
    start: Instant,
) -> CommandResult {
    let path = request.path.as_str();
    let payload = match parse_json_payload(request) {
        Ok(payload) => payload,
        Err(e) => return line_edit_stdout(upload_error(None, None, e), start),
    };
    if let Err(e) = validate_artifact_runner_path(path) {
        return line_edit_stdout(upload_error(Some(path), None, e), start);
    }
    let root = match project_root(request) {
        Ok(root) => root,
        Err(e) => return line_edit_stdout(upload_error(Some(path), None, e), start),
    };
    let max_bytes = match parse_usize_field(&payload, "max_bytes", MAX_ARTIFACT_UPLOAD_BYTES) {
        Ok(value) if value > 0 => value,
        Ok(_) => {
            return line_edit_stdout(
                upload_error(Some(path), None, "max_bytes must be >= 1"),
                start,
            )
        }
        Err(e) => return line_edit_stdout(upload_error(Some(path), None, e), start),
    };
    if max_bytes > MAX_ARTIFACT_UPLOAD_BYTES {
        return line_edit_stdout(
            upload_error(
                Some(path),
                None,
                format!("max_bytes exceeds upload maximum ({MAX_ARTIFACT_UPLOAD_BYTES})"),
            ),
            start,
        );
    }
    let expected_bytes = match parse_optional_usize_field(&payload, "expected_bytes") {
        Ok(value) => value,
        Err(e) => return line_edit_stdout(upload_error(Some(path), None, e), start),
    };
    if expected_bytes.is_some_and(|bytes| bytes > max_bytes) {
        return line_edit_stdout(
            upload_error(Some(path), None, "expected_bytes exceeds max_bytes"),
            start,
        );
    }
    let expected_sha256 = match parse_optional_clean_string(&payload, "expected_sha256", 64) {
        Ok(value) => value,
        Err(e) => return line_edit_stdout(upload_error(Some(path), None, e), start),
    };
    if expected_sha256
        .as_deref()
        .is_some_and(|sha256| !is_hex_sha256(sha256))
    {
        return line_edit_stdout(
            upload_error(
                Some(path),
                None,
                "expected_sha256 must be a lowercase 64-char hex sha256 digest",
            ),
            start,
        );
    }
    let mime_type = match parse_optional_clean_string(&payload, "mime_type", 128) {
        Ok(value) => value,
        Err(e) => return line_edit_stdout(upload_error(Some(path), None, e), start),
    };
    if matches!(mime_type.as_deref(), Some("application/octet-stream"))
        && !has_safe_octet_stream_artifact_extension(path)
    {
        return line_edit_stdout(
            upload_policy_rejected_error(Some(path), None, octet_stream_safe_extension_error()),
            start,
        );
    }
    let overwrite = match parse_bool_field(&payload, "overwrite") {
        Ok(value) => value,
        Err(e) => return line_edit_stdout(upload_error(Some(path), None, e), start),
    };

    let exists = std::fs::symlink_metadata(resolved).is_ok();
    if exists && !overwrite {
        return line_edit_stdout(
            upload_error(Some(path), None, "file exists and overwrite is false"),
            start,
        );
    }
    if exists
        && std::fs::symlink_metadata(resolved)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
    {
        return line_edit_stdout(
            upload_error(
                Some(path),
                None,
                "refusing to overwrite symlink artifact path",
            ),
            start,
        );
    }
    if let Err(e) = ensure_parent_in_project_root(resolved, &root) {
        return line_edit_stdout(upload_error(Some(path), None, e), start);
    }
    if std::fs::symlink_metadata(resolved)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return line_edit_stdout(
            upload_error(
                Some(path),
                None,
                "refusing to overwrite symlink artifact path",
            ),
            start,
        );
    }
    let parent = match resolved.parent() {
        Some(parent) => parent,
        None => {
            return line_edit_stdout(
                upload_error(Some(path), None, "target path has no parent directory"),
                start,
            )
        }
    };
    let usage = match current_artifact_upload_project_usage(&root) {
        Ok(usage) => usage,
        Err(e) => return line_edit_stdout(upload_error(Some(path), None, e), start),
    };
    if let Err(e) = enforce_artifact_upload_begin_admission(&usage, max_bytes) {
        return line_edit_stdout(upload_error(Some(path), None, e), start);
    }
    let state = ArtifactUploadState {
        path: path.to_string(),
        expected_bytes,
        expected_sha256,
        mime_type,
        overwrite,
        max_bytes,
    };
    let mut last_error = None;
    for attempt in 0..16 {
        let upload_id = new_upload_id(attempt);
        let (part, sidecar) = upload_paths(parent, &upload_id);
        if sidecar.exists() {
            continue;
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
        {
            Ok(file) => {
                if let Err(e) = file.sync_all() {
                    let _ = std::fs::remove_file(&part);
                    return line_edit_stdout(
                        upload_error(
                            Some(path),
                            Some(&upload_id),
                            format!("upload begin failed: {e}"),
                        ),
                        start,
                    );
                }
                drop(file);
                if let Err(e) = write_upload_state(&sidecar, &state) {
                    let _ = std::fs::remove_file(&part);
                    return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start);
                }
                if let Ok(dir) = std::fs::File::open(parent) {
                    let _ = dir.sync_all();
                }
                return line_edit_stdout(
                    json!({
                        "upload_id": upload_id,
                        "path": path,
                        "received_bytes": 0,
                        "next_offset": 0,
                        "expected_bytes": state.expected_bytes,
                        "expected_sha256": state.expected_sha256,
                        "max_bytes": state.max_bytes,
                        "mime_type": state.mime_type,
                        "overwrite": state.overwrite,
                        "committed": false,
                    }),
                    start,
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(e.to_string());
            }
            Err(e) => {
                return line_edit_stdout(
                    upload_error(Some(path), None, format!("upload begin failed: {e}")),
                    start,
                )
            }
        }
    }
    line_edit_stdout(
        upload_error(
            Some(path),
            None,
            last_error.unwrap_or_else(|| "could not create upload session".to_string()),
        ),
        start,
    )
}

fn handle_artifact_upload_chunk(
    request: &RunnerFilePayload,
    resolved: &Path,
    start: Instant,
) -> CommandResult {
    let path = request.path.as_str();
    let payload = match parse_json_payload(request) {
        Ok(payload) => payload,
        Err(e) => return line_edit_stdout(upload_error(None, None, e), start),
    };
    let upload_id = match parse_required_clean_string(&payload, "upload_id", 96) {
        Ok(value) => value,
        Err(e) => return line_edit_stdout(upload_error(Some(path), None, e), start),
    };
    if let Err(e) = validate_upload_id(&upload_id) {
        return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start);
    }
    if let Err(e) = validate_artifact_runner_path(path) {
        return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start);
    }
    let offset = match parse_optional_usize_field(&payload, "offset") {
        Ok(Some(value)) => value,
        Ok(None) => {
            return line_edit_stdout(
                upload_error(Some(path), Some(&upload_id), "offset is required"),
                start,
            )
        }
        Err(e) => return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start),
    };
    let max_chunk_bytes =
        match parse_usize_field(&payload, "max_chunk_bytes", MAX_ARTIFACT_UPLOAD_CHUNK_BYTES) {
            Ok(value) if value > 0 => value,
            Ok(_) => {
                return line_edit_stdout(
                    upload_error(Some(path), Some(&upload_id), "max_chunk_bytes must be >= 1"),
                    start,
                )
            }
            Err(e) => {
                return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start)
            }
        };
    if max_chunk_bytes > MAX_ARTIFACT_UPLOAD_CHUNK_BYTES {
        return line_edit_stdout(
            upload_error(
                Some(path),
                Some(&upload_id),
                format!(
                    "max_chunk_bytes exceeds upload chunk maximum ({MAX_ARTIFACT_UPLOAD_CHUNK_BYTES})"
                ),
            ),
            start,
        );
    }
    let content_base64 = match payload.get("content_base64").and_then(Value::as_str) {
        Some(value) if !value.contains('\0') => value,
        _ => {
            return line_edit_stdout(
                upload_error(
                    Some(path),
                    Some(&upload_id),
                    "content_base64 must be a base64 string without NUL",
                ),
                start,
            )
        }
    };
    let data = match general_purpose::STANDARD.decode(content_base64.as_bytes()) {
        Ok(data) => data,
        Err(e) => {
            return line_edit_stdout(
                upload_error(Some(path), Some(&upload_id), format!("invalid base64: {e}")),
                start,
            )
        }
    };
    if data.is_empty() {
        return line_edit_stdout(
            upload_error(
                Some(path),
                Some(&upload_id),
                "decoded chunk must contain at least 1 byte",
            ),
            start,
        );
    }
    if data.len() > max_chunk_bytes {
        return line_edit_stdout(
            upload_error(Some(path), Some(&upload_id), "decoded chunk too large"),
            start,
        );
    }
    let root = match project_root(request) {
        Ok(root) => root,
        Err(e) => return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start),
    };
    if let Err(e) = ensure_existing_parent_in_project_root(resolved, &root) {
        return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start);
    }
    let parent = match resolved.parent() {
        Some(parent) => parent,
        None => {
            return line_edit_stdout(
                upload_error(
                    Some(path),
                    Some(&upload_id),
                    "target path has no parent directory",
                ),
                start,
            )
        }
    };
    let (part, sidecar) = upload_paths(parent, &upload_id);
    let state = match read_upload_state(&sidecar, path) {
        Ok(state) => state,
        Err(e) => return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start),
    };
    if state.max_bytes > MAX_ARTIFACT_UPLOAD_BYTES {
        return line_edit_stdout(
            upload_error(
                Some(path),
                Some(&upload_id),
                "upload max_bytes exceeds per-file upload maximum",
            ),
            start,
        );
    }
    let received_bytes = match std::fs::metadata(&part) {
        Ok(metadata) => metadata.len() as usize,
        Err(e) => {
            return line_edit_stdout(
                upload_error(
                    Some(path),
                    Some(&upload_id),
                    format!("upload chunk failed: {e}"),
                ),
                start,
            )
        }
    };
    if received_bytes != offset {
        return line_edit_stdout(
            json!({
                "path": path,
                "upload_id": upload_id,
                "received_bytes": received_bytes,
                "next_offset": received_bytes,
                "expected_bytes": state.expected_bytes,
                "expected_sha256": state.expected_sha256,
                "max_bytes": state.max_bytes,
                "mime_type": state.mime_type,
                "committed": false,
                "error": "offset does not match received_bytes",
            }),
            start,
        );
    }
    let next_offset = match received_bytes.checked_add(data.len()) {
        Some(value) => value,
        None => {
            return line_edit_stdout(
                upload_error(Some(path), Some(&upload_id), "upload size overflow"),
                start,
            )
        }
    };
    if next_offset > state.max_bytes {
        return line_edit_stdout(
            upload_error(Some(path), Some(&upload_id), "upload exceeds max_bytes"),
            start,
        );
    }
    let mut file = match std::fs::OpenOptions::new().append(true).open(&part) {
        Ok(file) => file,
        Err(e) => {
            return line_edit_stdout(
                upload_error(
                    Some(path),
                    Some(&upload_id),
                    format!("upload chunk failed: {e}"),
                ),
                start,
            )
        }
    };
    if let Err(e) = file.write_all(&data) {
        return line_edit_stdout(
            upload_error(
                Some(path),
                Some(&upload_id),
                format!("upload chunk failed: {e}"),
            ),
            start,
        );
    }
    if let Err(e) = file.sync_all() {
        return line_edit_stdout(
            upload_error(
                Some(path),
                Some(&upload_id),
                format!("upload chunk failed: {e}"),
            ),
            start,
        );
    }
    line_edit_stdout(
        json!({
            "path": path,
            "upload_id": upload_id,
            "received_bytes": next_offset,
            "next_offset": next_offset,
            "expected_bytes": state.expected_bytes,
            "expected_sha256": state.expected_sha256,
            "max_bytes": state.max_bytes,
            "mime_type": state.mime_type,
            "committed": false,
        }),
        start,
    )
}

fn handle_artifact_upload_finish(
    request: &RunnerFilePayload,
    resolved: &Path,
    start: Instant,
) -> CommandResult {
    let path = request.path.as_str();
    let payload = match parse_json_payload(request) {
        Ok(payload) => payload,
        Err(e) => return line_edit_stdout(upload_error(None, None, e), start),
    };
    let upload_id = match parse_required_clean_string(&payload, "upload_id", 96) {
        Ok(value) => value,
        Err(e) => return line_edit_stdout(upload_error(Some(path), None, e), start),
    };
    if let Err(e) = validate_upload_id(&upload_id) {
        return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start);
    }
    if let Err(e) = validate_artifact_runner_path(path) {
        return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start);
    }
    let root = match project_root(request) {
        Ok(root) => root,
        Err(e) => return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start),
    };
    if let Err(e) = ensure_existing_parent_in_project_root(resolved, &root) {
        return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start);
    }
    let parent = match resolved.parent() {
        Some(parent) => parent,
        None => {
            return line_edit_stdout(
                upload_error(
                    Some(path),
                    Some(&upload_id),
                    "target path has no parent directory",
                ),
                start,
            )
        }
    };
    let (part, sidecar) = upload_paths(parent, &upload_id);
    let state = match read_upload_state(&sidecar, path) {
        Ok(state) => state,
        Err(e) => return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start),
    };
    let (bytes, sha256) = match verify_upload_file(&part, state.max_bytes) {
        Ok(verification) => verification,
        Err(e) => return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start),
    };
    if state
        .expected_bytes
        .is_some_and(|expected| expected != bytes)
    {
        return line_edit_stdout(
            json!({
                "path": path,
                "upload_id": upload_id,
                "received_bytes": bytes,
                "expected_bytes": state.expected_bytes,
                "expected_sha256": state.expected_sha256,
                "sha256": sha256,
                "mime_type": state.mime_type,
                "committed": false,
                "error": "uploaded byte count does not match expected_bytes",
            }),
            start,
        );
    }
    if state
        .expected_sha256
        .as_deref()
        .is_some_and(|expected| expected != sha256)
    {
        return line_edit_stdout(
            json!({
                "path": path,
                "upload_id": upload_id,
                "received_bytes": bytes,
                "expected_bytes": state.expected_bytes,
                "expected_sha256": state.expected_sha256,
                "sha256": sha256,
                "mime_type": state.mime_type,
                "committed": false,
                "error": "uploaded sha256 does not match expected_sha256",
            }),
            start,
        );
    }
    let detected_mime = if state.mime_type.is_none() {
        artifact_mime_from_file(path, &part, true)
    } else {
        None
    };
    let exists = std::fs::symlink_metadata(resolved).is_ok();
    if exists && !state.overwrite {
        return line_edit_stdout(
            upload_error(
                Some(path),
                Some(&upload_id),
                "file exists and overwrite is false",
            ),
            start,
        );
    }
    if exists
        && std::fs::symlink_metadata(resolved)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
    {
        return line_edit_stdout(
            upload_error(
                Some(path),
                Some(&upload_id),
                "refusing to overwrite symlink artifact path",
            ),
            start,
        );
    }
    if let Err(e) = commit_artifact_upload_part(&part, resolved, state.overwrite) {
        return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start);
    }
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    let _ = std::fs::remove_file(&sidecar);
    line_edit_stdout(
        json!({
            "path": path,
            "upload_id": upload_id,
            "bytes": bytes,
            "received_bytes": bytes,
            "expected_bytes": state.expected_bytes,
            "expected_sha256": state.expected_sha256,
            "sha256": sha256,
            "mime_type": state.mime_type.or(detected_mime),
            "committed": true,
        }),
        start,
    )
}

fn handle_artifact_upload_abort(
    request: &RunnerFilePayload,
    resolved: &Path,
    start: Instant,
) -> CommandResult {
    let path = request.path.as_str();
    let payload = match parse_json_payload(request) {
        Ok(payload) => payload,
        Err(e) => return line_edit_stdout(upload_error(None, None, e), start),
    };
    let upload_id = match parse_required_clean_string(&payload, "upload_id", 96) {
        Ok(value) => value,
        Err(e) => return line_edit_stdout(upload_error(Some(path), None, e), start),
    };
    if let Err(e) = validate_upload_id(&upload_id) {
        return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start);
    }
    if let Err(e) = validate_artifact_runner_path(path) {
        return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start);
    }
    let root = match project_root(request) {
        Ok(root) => root,
        Err(e) => return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start),
    };
    if let Err(e) = ensure_existing_parent_in_project_root(resolved, &root) {
        return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start);
    }
    let parent = match resolved.parent() {
        Some(parent) => parent,
        None => {
            return line_edit_stdout(
                upload_error(
                    Some(path),
                    Some(&upload_id),
                    "target path has no parent directory",
                ),
                start,
            )
        }
    };
    let (part, sidecar) = upload_paths(parent, &upload_id);
    let state = match read_upload_state(&sidecar, path) {
        Ok(state) => state,
        Err(e) => return line_edit_stdout(upload_error(Some(path), Some(&upload_id), e), start),
    };
    let received_bytes = std::fs::metadata(&part)
        .map(|metadata| metadata.len() as usize)
        .unwrap_or(0);
    let temp_file_removed = std::fs::remove_file(&part).is_ok();
    let sidecar_removed = std::fs::remove_file(&sidecar).is_ok();
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    let final_file_exists = std::fs::symlink_metadata(resolved).is_ok();
    let changed_status = if final_file_exists {
        "upload_aborted_final_file_preexisting"
    } else {
        "upload_aborted_no_final_file"
    };
    line_edit_stdout(
        json!({
            "path": path,
            "upload_id": upload_id,
            "received_bytes": received_bytes,
            "expected_bytes": state.expected_bytes,
            "expected_sha256": state.expected_sha256,
            "mime_type": state.mime_type,
            "committed": false,
            "aborted": true,
            "temp_file_removed": temp_file_removed,
            "sidecar_removed": sidecar_removed,
            "final_file_touched": false,
            "final_file_exists": final_file_exists,
            "changed_path_details": [{
                "path": path,
                "status": changed_status,
            }],
        }),
        start,
    )
}

fn handle_read_project_artifact_metadata(
    request: &RunnerFilePayload,
    resolved: &Path,
    start: Instant,
) -> CommandResult {
    let path = request.path.as_str();
    let payload = match parse_json_payload(request) {
        Ok(payload) => payload,
        Err(e) => return line_edit_stdout(metadata_error(None, e), start),
    };
    if let Err(e) = validate_artifact_runner_path(path) {
        return line_edit_stdout(metadata_error(Some(path), e), start);
    }
    let root = match project_root(request) {
        Ok(root) => root,
        Err(e) => return line_edit_stdout(metadata_error(Some(path), e), start),
    };
    let allow_missing = match parse_bool_field(&payload, "allow_missing") {
        Ok(value) => value,
        Err(e) => return line_edit_stdout(metadata_error(Some(path), e), start),
    };
    if let Err(e) = ensure_existing_target_in_project_root(resolved, &root) {
        let target_missing = matches!(
            std::fs::symlink_metadata(resolved),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound
        );
        if allow_missing && target_missing {
            return line_edit_stdout(
                json!({
                    "path": path,
                    "exists": false,
                    "missing": true,
                }),
                start,
            );
        }
        return line_edit_stdout(metadata_error(Some(path), e), start);
    }
    let max_bytes = match parse_usize_field(&payload, "max_bytes", DEFAULT_MAX_ARTIFACT_BYTES) {
        Ok(value) => value,
        Err(e) => return line_edit_stdout(metadata_error(Some(path), e), start),
    };
    if max_bytes > MAX_ARTIFACT_EXPORT_BYTES {
        return line_edit_stdout(
            metadata_error(
                Some(path),
                format!("artifact metadata maximum exceeds {MAX_ARTIFACT_EXPORT_BYTES} bytes"),
            ),
            start,
        );
    }
    if max_bytes > DEFAULT_MAX_ARTIFACT_BYTES {
        let initial_bytes = match std::fs::metadata(resolved)
            .ok()
            .and_then(|metadata| usize::try_from(metadata.len()).ok())
        {
            Some(bytes) => bytes,
            None => {
                return line_edit_stdout(
                    metadata_error(Some(path), "artifact size does not fit this platform"),
                    start,
                )
            }
        };
        if initial_bytes > max_bytes {
            return line_edit_stdout(
                metadata_error(Some(path), "artifact too large to inspect"),
                start,
            );
        }
        let (bytes, sha256) = match verify_upload_file(resolved, max_bytes) {
            Ok(verification) => verification,
            Err(e) => return line_edit_stdout(metadata_error(Some(path), e), start),
        };
        let final_metadata = match std::fs::metadata(resolved) {
            Ok(metadata) => metadata,
            Err(e) => {
                return line_edit_stdout(
                    metadata_error(Some(path), format!("stat failed: {e}")),
                    start,
                )
            }
        };
        let final_bytes = match usize::try_from(final_metadata.len()) {
            Ok(bytes) => bytes,
            Err(_) => {
                return line_edit_stdout(
                    metadata_error(Some(path), "artifact size does not fit this platform"),
                    start,
                )
            }
        };
        if bytes != initial_bytes || bytes != final_bytes {
            return line_edit_stdout(
                metadata_error(
                    Some(path),
                    "artifact size changed during metadata inspection",
                ),
                start,
            );
        }
        let mime_type = artifact_mime_from_file(path, resolved, false);
        let mut out = json!({
            "path": path,
            "exists": true,
            "missing": false,
            "bytes": bytes,
            "sha256": sha256,
            "mime_type": mime_type,
        });
        if let Ok(modified) = final_metadata.modified().and_then(|modified| {
            modified
                .duration_since(UNIX_EPOCH)
                .map_err(std::io::Error::other)
        }) {
            out["modified_at"] = json!(modified.as_secs());
        }
        return line_edit_stdout(out, start);
    }
    let data = match read_limited(resolved, max_bytes) {
        Ok(data) => data,
        Err(e) => return line_edit_stdout(metadata_error(Some(path), e), start),
    };
    let mime_type = artifact_mime(path, &data, false);
    let mut out = json!({
        "path": path,
        "exists": true,
        "missing": false,
        "bytes": data.len(),
        "sha256": sha256_hex_bytes(&data),
        "mime_type": mime_type,
    });
    if let Ok(modified) = std::fs::metadata(resolved)
        .and_then(|metadata| metadata.modified())
        .and_then(|modified| {
            modified
                .duration_since(UNIX_EPOCH)
                .map_err(std::io::Error::other)
        })
    {
        out["modified_at"] = json!(modified.as_secs());
    }
    if let Some((width, height)) = image_size(&data) {
        out["width"] = json!(width);
        out["height"] = json!(height);
    }
    if out["mime_type"].as_str() == Some("application/zip") {
        out["archive_entries_count"] = json!(zip_entry_count(&data));
    }
    line_edit_stdout(out, start)
}

fn handle_read_project_artifact_export_chunk(
    request: &RunnerFilePayload,
    resolved: &Path,
    start: Instant,
) -> CommandResult {
    let path = request.path.as_str();
    let payload = match parse_json_payload(request) {
        Ok(payload) => payload,
        Err(e) => return line_edit_stdout(read_error(None, e), start),
    };
    if let Err(e) = validate_artifact_runner_path(path) {
        return line_edit_stdout(read_error(Some(path), e), start);
    }
    let root = match project_root(request) {
        Ok(root) => root,
        Err(e) => return line_edit_stdout(read_error(Some(path), e), start),
    };
    if let Err(e) = ensure_existing_target_in_project_root(resolved, &root) {
        let msg = e.replacen("read failed", "stat failed", 1);
        return line_edit_stdout(read_error(Some(path), msg), start);
    }
    if payload.get("expected_file_bytes").is_none() {
        return line_edit_stdout(
            read_error(Some(path), "expected_file_bytes is required"),
            start,
        );
    }
    let expected_file_bytes = match parse_usize_field(&payload, "expected_file_bytes", 0) {
        Ok(value) => value,
        Err(e) => return line_edit_stdout(read_error(Some(path), e), start),
    };
    if expected_file_bytes > MAX_ARTIFACT_EXPORT_BYTES {
        return line_edit_stdout(
            read_error(
                Some(path),
                format!(
                    "artifact is too large to export; maximum is {} bytes",
                    MAX_ARTIFACT_EXPORT_BYTES
                ),
            ),
            start,
        );
    }
    let offset = match parse_usize_field(&payload, "offset", 0) {
        Ok(value) => value,
        Err(e) => return line_edit_stdout(read_error(Some(path), e), start),
    };
    let length = match parse_usize_field(&payload, "length", DEFAULT_ARTIFACT_READ_LENGTH) {
        Ok(value) => value,
        Err(e) => return line_edit_stdout(read_error(Some(path), e), start),
    };
    if length == 0 || length > MAX_ARTIFACT_EXPORT_CHUNK_BYTES {
        return line_edit_stdout(
            read_error(
                Some(path),
                format!(
                    "length must be between 1 and {} bytes",
                    MAX_ARTIFACT_EXPORT_CHUNK_BYTES
                ),
            ),
            start,
        );
    }
    let mut file = match std::fs::File::open(resolved) {
        Ok(file) => file,
        Err(e) => {
            return line_edit_stdout(read_error(Some(path), format!("read failed: {e}")), start)
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(e) => {
            return line_edit_stdout(read_error(Some(path), format!("stat failed: {e}")), start)
        }
    };
    let file_bytes = match usize::try_from(metadata.len()) {
        Ok(value) => value,
        Err(_) => {
            return line_edit_stdout(
                read_error(Some(path), "artifact size does not fit this platform"),
                start,
            )
        }
    };
    if file_bytes > MAX_ARTIFACT_EXPORT_BYTES {
        return line_edit_stdout(
            read_error(
                Some(path),
                format!(
                    "artifact is too large to export; maximum is {} bytes",
                    MAX_ARTIFACT_EXPORT_BYTES
                ),
            ),
            start,
        );
    }
    if file_bytes != expected_file_bytes {
        let mut output = read_error(
            Some(path),
            format!(
                "artifact size changed during export; expected {expected_file_bytes} bytes, found {file_bytes}"
            ),
        );
        output["error_kind"] = json!("snapshot_changed");
        return line_edit_stdout(output, start);
    }
    if offset > file_bytes {
        return line_edit_stdout(
            read_error(Some(path), "offset exceeds artifact size"),
            start,
        );
    }
    let requested_end = match offset.checked_add(length) {
        Some(value) => value,
        None => return line_edit_stdout(read_error(Some(path), "offset + length overflow"), start),
    };
    let next_offset = requested_end.min(file_bytes);
    let bytes_to_read = next_offset - offset;
    if let Err(e) = file.seek(SeekFrom::Start(offset as u64)) {
        return line_edit_stdout(read_error(Some(path), format!("seek failed: {e}")), start);
    }
    let mut segment = vec![0_u8; bytes_to_read];
    if let Err(e) = file.read_exact(&mut segment) {
        return line_edit_stdout(read_error(Some(path), format!("read failed: {e}")), start);
    }
    let truncated = next_offset < file_bytes;
    line_edit_stdout(
        json!({
            "path": path,
            "file_bytes": file_bytes,
            "offset": offset,
            "bytes_returned": segment.len(),
            "content_base64": general_purpose::STANDARD.encode(segment),
            "next_offset": next_offset,
            "truncated": truncated,
            "eof": !truncated,
        }),
        start,
    )
}

fn handle_read_project_artifact(
    request: &RunnerFilePayload,
    resolved: &Path,
    start: Instant,
) -> CommandResult {
    let path = request.path.as_str();
    let payload = match parse_json_payload(request) {
        Ok(payload) => payload,
        Err(e) => return line_edit_stdout(read_error(None, e), start),
    };
    if let Err(e) = validate_artifact_runner_path(path) {
        return line_edit_stdout(read_error(Some(path), e), start);
    }
    let root = match project_root(request) {
        Ok(root) => root,
        Err(e) => return line_edit_stdout(read_error(Some(path), e), start),
    };
    if let Err(e) = ensure_existing_target_in_project_root(resolved, &root) {
        let msg = e.replacen("read failed", "stat failed", 1);
        return line_edit_stdout(read_error(Some(path), msg), start);
    }
    let offset = match parse_usize_field(&payload, "offset", 0) {
        Ok(value) => value,
        Err(e) => return line_edit_stdout(read_error(Some(path), e), start),
    };
    let mcp_image = match parse_bool_field(&payload, "mcp_image") {
        Ok(value) => value,
        Err(e) => return line_edit_stdout(read_error(Some(path), e), start),
    };
    if mcp_image && offset != 0 {
        return line_edit_stdout(
            read_error(Some(path), "MCP image reads must start at offset 0"),
            start,
        );
    }
    let requested_length = match parse_usize_field(&payload, "length", DEFAULT_ARTIFACT_READ_LENGTH)
    {
        Ok(value) => value,
        Err(e) => return line_edit_stdout(read_error(Some(path), e), start),
    };
    let length = if mcp_image {
        MAX_MCP_IMAGE_BYTES
    } else {
        requested_length
    };
    if length < 1 {
        return line_edit_stdout(read_error(Some(path), "length must be >= 1"), start);
    }
    let requested_max_file_bytes =
        match parse_usize_field(&payload, "max_file_bytes", DEFAULT_MAX_ARTIFACT_BYTES) {
            Ok(value) => value,
            Err(e) => return line_edit_stdout(read_error(Some(path), e), start),
        };
    let max_file_bytes = if mcp_image {
        requested_max_file_bytes.min(MAX_MCP_IMAGE_BYTES)
    } else {
        requested_max_file_bytes
    };
    if max_file_bytes < 1 {
        return line_edit_stdout(read_error(Some(path), "max_file_bytes must be >= 1"), start);
    }
    let file_bytes = match std::fs::metadata(resolved) {
        Ok(metadata) => metadata.len(),
        Err(e) => {
            return line_edit_stdout(read_error(Some(path), format!("stat failed: {e}")), start)
        }
    };
    if file_bytes > max_file_bytes as u64 {
        let message = if mcp_image {
            format!(
                "MCP image too large; maximum is {} bytes",
                MAX_MCP_IMAGE_BYTES
            )
        } else {
            "artifact too large to read; use metadata or a smaller artifact".to_string()
        };
        return line_edit_stdout(read_error(Some(path), message), start);
    }
    let data = match std::fs::read(resolved) {
        Ok(data) => data,
        Err(e) => {
            return line_edit_stdout(read_error(Some(path), format!("read failed: {e}")), start)
        }
    };
    let mime_type = if mcp_image {
        match magic_mime(&data) {
            Some(mime @ ("image/png" | "image/jpeg" | "image/webp")) => Some(mime.to_string()),
            Some(mime) => {
                return line_edit_stdout(
                    read_error(
                        Some(path),
                        format!(
                            "unsupported MCP image MIME type '{mime}'; supported types are image/png, image/jpeg, and image/webp"
                        ),
                    ),
                    start,
                )
            }
            None => {
                return line_edit_stdout(
                    read_error(
                        Some(path),
                        "artifact content is not a supported PNG, JPEG, or WebP image",
                    ),
                    start,
                )
            }
        }
    } else {
        artifact_mime(path, &data, true)
    };
    let file_bytes = data.len();
    let (segment, next_offset, truncated) = if offset >= file_bytes {
        (&[][..], file_bytes, false)
    } else {
        let next_offset = offset.saturating_add(length).min(file_bytes);
        (
            &data[offset..next_offset],
            next_offset,
            next_offset < file_bytes,
        )
    };
    line_edit_stdout(
        json!({
            "path": path,
            "mime_type": mime_type,
            "file_bytes": file_bytes,
            "sha256": sha256_hex_bytes(&data),
            "offset": offset,
            "bytes_returned": segment.len(),
            "content_base64": general_purpose::STANDARD.encode(segment),
            "next_offset": next_offset,
            "truncated": truncated,
            "eof": !truncated,
        }),
        start,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_runner_path_validation_is_cross_platform_and_uses_shared_sensitive_policy() {
        assert!(validate_artifact_runner_path("artifacts/report.bin").is_ok());
        for path in [
            "/absolute/report.bin",
            "\\rooted\\report.bin",
            "C:\\absolute\\report.bin",
            "C:drive-relative\\report.bin",
            "../report.bin",
            "nested\\..\\report.bin",
            ".git\\config",
            "secrets\\token.bin",
            "certs\\server.key",
            "config\\runner.toml",
            "project-registry\\demo.toml",
        ] {
            assert!(
                validate_artifact_runner_path(path).is_err(),
                "{path} should be rejected"
            );
        }
    }

    #[test]
    fn atomic_artifact_write_create_only_never_replaces_existing_target() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("artifact.bin");

        write_bytes_atomic_strict(&path, b"first", false).unwrap();
        let error = write_bytes_atomic_strict(&path, b"second", false).unwrap_err();
        assert!(error.contains("overwrite is false"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), b"first");

        write_bytes_atomic_strict(&path, b"third", true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"third");
    }

    #[test]
    fn artifact_upload_create_only_commit_never_replaces_existing_target() {
        let temp = tempfile::tempdir().unwrap();
        let part = temp.path().join(".wc-upload-test.part");
        let target = temp.path().join("artifact.bin");
        std::fs::write(&part, b"upload").unwrap();
        std::fs::write(&target, b"concurrent").unwrap();

        let error = commit_artifact_upload_part(&part, &target, false).unwrap_err();
        assert_eq!(error, "file exists and overwrite is false");
        assert_eq!(std::fs::read(&target).unwrap(), b"concurrent");
        assert_eq!(std::fs::read(&part).unwrap(), b"upload");

        std::fs::remove_file(&target).unwrap();
        commit_artifact_upload_part(&part, &target, false).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"upload");
        assert!(!part.exists());
    }

    fn artifact_request(root: &Path, kind: &str, path: &str, payload: Value) -> RunnerRequest {
        RunnerRequest {
            request_id: format!("req-{kind}"),
            client_id: "agent-1".to_string(),
            kind: kind.to_string(),
            job_id: None,
            cwd: Some(root.to_string_lossy().to_string()),
            path: Some(path.to_string()),
            content: Some(payload.to_string()),
            max_bytes: None,
            expected_sha256: None,
            expected_prefix: None,
            start_line: None,
            end_line: None,
            create_dirs: false,
            command: String::new(),
            process: None,
            script: None,
            stdin: None,
            timeout_secs: 30,
            requested_by: "tester".to_string(),
            created_at: 0,
            validation: None,
            lsp: None,
            job_context: None,
            mcp_gateway: None,
            plugin_gateway: None,
            coding_agent: None,
            persistent_shell: None,
        }
    }

    fn artifact_output(result: CommandResult) -> Value {
        assert_eq!(result.exit_code, Some(0), "unexpected result: {result:?}");
        assert!(
            result.error.is_none(),
            "unexpected error: {:?}",
            result.error
        );
        serde_json::from_str(result.stdout.as_deref().expect("json stdout")).unwrap()
    }

    fn run_artifact_request(root: &Path, kind: &str, path: &str, payload: Value) -> Value {
        let request = artifact_request(root, kind, path, payload);
        let resolved = root.join(path);
        artifact_output(handle_artifact_file_request(
            &request,
            &resolved,
            Instant::now(),
        ))
    }

    fn test_upload_state(path: &str) -> ArtifactUploadState {
        ArtifactUploadState {
            path: path.to_string(),
            expected_bytes: None,
            expected_sha256: None,
            mime_type: None,
            overwrite: false,
            max_bytes: MAX_ARTIFACT_UPLOAD_BYTES,
        }
    }

    fn write_test_upload_pair(parent: &Path, upload_id: &str, path: &str, bytes: &[u8]) {
        let (part, sidecar) = upload_paths(parent, upload_id);
        std::fs::write(&part, bytes).unwrap();
        write_upload_state(&sidecar, &test_upload_state(path)).unwrap();
    }

    #[test]
    fn verify_upload_file_streams_across_multiple_buffers() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("large.part");
        let bytes = vec![b'x'; ARTIFACT_STREAM_BUFFER_BYTES * 2 + 17];
        std::fs::write(&path, &bytes).unwrap();

        let (verified_bytes, sha256) = verify_upload_file(&path, bytes.len()).unwrap();
        assert_eq!(verified_bytes, bytes.len());
        assert_eq!(sha256, sha256_hex_bytes(&bytes));
        assert_eq!(
            verify_upload_file(&path, bytes.len() - 1).unwrap_err(),
            "artifact too large to inspect"
        );
    }

    #[test]
    fn artifact_upload_project_sweep_cleans_orphans_and_stale_pairs() {
        let tmp = tempfile::tempdir().unwrap();
        let active_parent = tmp.path().join("artifacts/active");
        let orphan_parent = tmp.path().join("artifacts/orphans");
        std::fs::create_dir_all(&active_parent).unwrap();
        std::fs::create_dir_all(&orphan_parent).unwrap();
        let active_id = "wc_upload_active";
        write_test_upload_pair(
            &active_parent,
            active_id,
            "artifacts/active/active.bin",
            b"abc",
        );

        let (orphan_part, _) = upload_paths(&orphan_parent, "wc_upload_orphan_part");
        std::fs::write(&orphan_part, b"orphan").unwrap();
        let (_, orphan_sidecar) = upload_paths(&orphan_parent, "wc_upload_orphan_sidecar");
        write_upload_state(
            &orphan_sidecar,
            &test_upload_state("artifacts/orphans/orphan.bin"),
        )
        .unwrap();

        let now = SystemTime::now();
        let usage =
            sweep_artifact_upload_project(tmp.path(), now, Duration::from_secs(60)).unwrap();
        assert_eq!(
            usage,
            ArtifactUploadProjectUsage {
                active_uploads: 1,
                reserved_bytes: MAX_ARTIFACT_UPLOAD_BYTES,
            }
        );
        assert!(!orphan_part.exists());
        assert!(!orphan_sidecar.exists());

        let future = now + Duration::from_secs(61);
        let expired =
            sweep_artifact_upload_project(tmp.path(), future, Duration::from_secs(60)).unwrap();
        assert_eq!(expired, ArtifactUploadProjectUsage::default());
        let (part, sidecar) = upload_paths(&active_parent, active_id);
        assert!(!part.exists());
        assert!(!sidecar.exists());
    }

    #[test]
    fn artifact_upload_project_limits_bound_count_and_reserved_bytes() {
        let at_count_limit = ArtifactUploadProjectUsage {
            active_uploads: MAX_ACTIVE_ARTIFACT_UPLOADS_PER_PROJECT,
            reserved_bytes: 0,
        };
        assert!(enforce_artifact_upload_begin_admission(&at_count_limit, 1)
            .unwrap_err()
            .contains("active upload limit"));

        let nearly_full = ArtifactUploadProjectUsage {
            active_uploads: 1,
            reserved_bytes: MAX_ARTIFACT_UPLOAD_RESERVED_BYTES_PER_PROJECT - 1,
        };
        enforce_artifact_upload_begin_admission(&nearly_full, 1).unwrap();
        assert!(enforce_artifact_upload_begin_admission(&nearly_full, 2)
            .unwrap_err()
            .contains("reserved byte quota exceeded"));
    }

    #[test]
    fn artifact_upload_begin_enforces_per_file_maximum() {
        let tmp = tempfile::tempdir().unwrap();
        let accepted_path = "artifacts/imports/max-file.bin";
        let accepted = run_artifact_request(
            tmp.path(),
            "file_artifact_upload_begin",
            accepted_path,
            json!({
                "path": accepted_path,
                "expected_bytes": null,
                "expected_sha256": null,
                "mime_type": null,
                "overwrite": false,
                "max_bytes": MAX_ARTIFACT_UPLOAD_BYTES,
            }),
        );
        assert_eq!(accepted["max_bytes"], MAX_ARTIFACT_UPLOAD_BYTES);
        let upload_id = accepted["upload_id"].as_str().unwrap().to_string();
        let aborted = run_artifact_request(
            tmp.path(),
            "file_artifact_upload_abort",
            accepted_path,
            json!({"path": accepted_path, "upload_id": upload_id}),
        );
        assert_eq!(aborted["aborted"], true);

        let rejected_path = "artifacts/imports/too-large-file.bin";
        let rejected = run_artifact_request(
            tmp.path(),
            "file_artifact_upload_begin",
            rejected_path,
            json!({
                "path": rejected_path,
                "expected_bytes": null,
                "expected_sha256": null,
                "mime_type": null,
                "overwrite": false,
                "max_bytes": MAX_ARTIFACT_UPLOAD_BYTES + 1,
            }),
        );
        assert_eq!(
            rejected["error"],
            format!("max_bytes exceeds upload maximum ({MAX_ARTIFACT_UPLOAD_BYTES})")
        );
    }

    #[test]
    fn artifact_upload_chunk_enforces_data_plane_maximum() {
        let tmp = tempfile::tempdir().unwrap();
        let path = "artifacts/imports/max-chunk.bin";
        let begin = run_artifact_request(
            tmp.path(),
            "file_artifact_upload_begin",
            path,
            json!({
                "path": path,
                "expected_bytes": null,
                "expected_sha256": null,
                "mime_type": null,
                "overwrite": false,
                "max_bytes": MAX_ARTIFACT_UPLOAD_CHUNK_BYTES * 2,
            }),
        );
        let upload_id = begin["upload_id"].as_str().unwrap().to_string();

        let rejected = run_artifact_request(
            tmp.path(),
            "file_artifact_upload_chunk",
            path,
            json!({
                "path": path,
                "upload_id": upload_id.clone(),
                "offset": 0,
                "content_base64": "YQ==",
                "max_chunk_bytes": MAX_ARTIFACT_UPLOAD_CHUNK_BYTES + 1,
            }),
        );
        assert_eq!(
            rejected["error"],
            format!(
                "max_chunk_bytes exceeds upload chunk maximum ({MAX_ARTIFACT_UPLOAD_CHUNK_BYTES})"
            )
        );
        let (part, _) = upload_paths(tmp.path().join("artifacts/imports").as_path(), &upload_id);
        assert_eq!(std::fs::metadata(&part).unwrap().len(), 0);

        let bytes = vec![b'x'; MAX_ARTIFACT_UPLOAD_CHUNK_BYTES];
        let accepted = run_artifact_request(
            tmp.path(),
            "file_artifact_upload_chunk",
            path,
            json!({
                "path": path,
                "upload_id": upload_id.clone(),
                "offset": 0,
                "content_base64": general_purpose::STANDARD.encode(&bytes),
                "max_chunk_bytes": MAX_ARTIFACT_UPLOAD_CHUNK_BYTES,
            }),
        );
        assert_eq!(accepted["received_bytes"], MAX_ARTIFACT_UPLOAD_CHUNK_BYTES);

        let aborted = run_artifact_request(
            tmp.path(),
            "file_artifact_upload_abort",
            path,
            json!({"path": path, "upload_id": upload_id}),
        );
        assert_eq!(aborted["aborted"], true);
    }

    #[test]
    fn artifact_upload_begin_rejects_project_active_upload_limit_across_directories() {
        let tmp = tempfile::tempdir().unwrap();
        for index in 0..MAX_ACTIVE_ARTIFACT_UPLOADS_PER_PROJECT {
            let parent = tmp.path().join(format!("artifacts/set-{index}"));
            std::fs::create_dir_all(&parent).unwrap();
            let upload_id = format!("wc_upload_limit_{index}");
            write_test_upload_pair(
                &parent,
                &upload_id,
                &format!("artifacts/set-{index}/existing.bin"),
                b"",
            );
        }

        let path = "artifacts/imports/new.bin";
        let output = run_artifact_request(
            tmp.path(),
            "file_artifact_upload_begin",
            path,
            json!({
                "path": path,
                "expected_bytes": null,
                "expected_sha256": null,
                "mime_type": null,
                "overwrite": false,
                "max_bytes": DEFAULT_MAX_ARTIFACT_BYTES,
            }),
        );
        assert!(output["error"]
            .as_str()
            .unwrap()
            .contains("active upload limit"));
    }

    #[test]
    fn artifact_upload_begin_rejects_project_reserved_byte_quota() {
        let tmp = tempfile::tempdir().unwrap();
        for index in 0..2 {
            let parent = tmp.path().join(format!("artifacts/quota-{index}"));
            std::fs::create_dir_all(&parent).unwrap();
            let upload_id = format!("wc_upload_quota_{index}");
            write_test_upload_pair(
                &parent,
                &upload_id,
                &format!("artifacts/quota-{index}/existing.bin"),
                b"",
            );
        }

        let path = "artifacts/imports/new.bin";
        let output = run_artifact_request(
            tmp.path(),
            "file_artifact_upload_begin",
            path,
            json!({
                "path": path,
                "expected_bytes": null,
                "expected_sha256": null,
                "mime_type": null,
                "overwrite": false,
                "max_bytes": DEFAULT_MAX_ARTIFACT_BYTES,
            }),
        );
        assert!(output["error"]
            .as_str()
            .unwrap()
            .contains("reserved byte quota exceeded"));
    }

    #[test]
    fn read_upload_state_rejects_requested_path_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let (_part, sidecar) = upload_paths(tmp.path(), "wc_upload_test_1");
        write_upload_state(
            &sidecar,
            &ArtifactUploadState {
                path: "artifacts/imports/a.bin".to_string(),
                expected_bytes: None,
                expected_sha256: None,
                mime_type: None,
                overwrite: false,
                max_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
            },
        )
        .unwrap();

        let err = read_upload_state(&sidecar, "artifacts/imports/b.bin").unwrap_err();
        assert_eq!(err, "upload_id does not belong to requested path");
    }

    #[test]
    fn read_upload_state_rejects_oversized_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, sidecar) = upload_paths(tmp.path(), "wc_upload_oversized_state");
        std::fs::write(&sidecar, vec![b'x'; MAX_ARTIFACT_UPLOAD_STATE_BYTES + 1]).unwrap();

        let err = read_upload_state_file(&sidecar).unwrap_err();
        assert_eq!(err, "invalid upload state: sidecar too large");
    }

    #[test]
    fn artifact_upload_chunk_rejects_legacy_oversized_reservation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = "artifacts/imports/legacy-oversized.bin";
        let parent = tmp.path().join("artifacts/imports");
        std::fs::create_dir_all(&parent).unwrap();
        let upload_id = "wc_upload_legacy_oversized";
        let (part, sidecar) = upload_paths(&parent, upload_id);
        std::fs::write(&part, b"").unwrap();
        write_upload_state(
            &sidecar,
            &ArtifactUploadState {
                path: path.to_string(),
                expected_bytes: None,
                expected_sha256: None,
                mime_type: None,
                overwrite: false,
                max_bytes: MAX_ARTIFACT_UPLOAD_BYTES + 1,
            },
        )
        .unwrap();

        let output = run_artifact_request(
            tmp.path(),
            "file_artifact_upload_chunk",
            path,
            json!({
                "path": path,
                "upload_id": upload_id,
                "offset": 0,
                "content_base64": "YQ==",
                "max_chunk_bytes": MAX_ARTIFACT_UPLOAD_CHUNK_BYTES,
            }),
        );
        assert_eq!(
            output["error"],
            "upload max_bytes exceeds per-file upload maximum"
        );
        assert_eq!(std::fs::metadata(&part).unwrap().len(), 0);
        assert!(sidecar.exists());
        assert!(!tmp.path().join(path).exists());
    }

    #[test]
    fn read_project_artifact_metadata_allow_missing_returns_successful_absence() {
        let tmp = tempfile::tempdir().unwrap();
        let path = "artifacts/smoke/missing.artifact";

        let missing = run_artifact_request(
            tmp.path(),
            "file_read_project_artifact_metadata",
            path,
            json!({"path": path, "allow_missing": true}),
        );

        assert_eq!(missing["path"], path);
        assert_eq!(missing["exists"], false);
        assert_eq!(missing["missing"], true);
        assert!(missing.get("error").is_none());
    }

    #[test]
    fn read_project_artifact_metadata_missing_without_allow_missing_keeps_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = "artifacts/smoke/missing.artifact";

        let missing = run_artifact_request(
            tmp.path(),
            "file_read_project_artifact_metadata",
            path,
            json!({"path": path}),
        );

        assert_eq!(missing["path"], path);
        assert!(missing["error"].as_str().unwrap().contains("read failed"));
        assert!(missing.get("exists").is_none());
    }

    #[test]
    fn read_project_artifact_metadata_existing_reports_exists_with_allow_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = "artifacts/smoke/existing.artifact";
        let resolved = tmp.path().join(path);
        std::fs::create_dir_all(resolved.parent().unwrap()).unwrap();
        std::fs::write(&resolved, b"hello").unwrap();

        let metadata = run_artifact_request(
            tmp.path(),
            "file_read_project_artifact_metadata",
            path,
            json!({"path": path, "allow_missing": true}),
        );

        assert_eq!(metadata["exists"], true);
        assert_eq!(metadata["missing"], false);
        assert_eq!(metadata["bytes"], 5);
        assert_eq!(metadata["sha256"], sha256_hex_bytes(b"hello"));
    }

    #[test]
    fn read_project_artifact_mcp_image_returns_complete_supported_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let cases: [(&str, &[u8], &str); 3] = [
            (
                "docs/images/sample.png",
                b"\x89PNG\r\n\x1a\npng-body",
                "image/png",
            ),
            (
                "docs/images/sample.jpg",
                b"\xff\xd8\xff\xe0jpeg-body\xff\xd9",
                "image/jpeg",
            ),
            (
                "docs/images/sample.webp",
                b"RIFF\x08\x00\x00\x00WEBPwebp-body",
                "image/webp",
            ),
        ];

        for (path, bytes, expected_mime) in cases {
            let resolved = tmp.path().join(path);
            std::fs::create_dir_all(resolved.parent().unwrap()).unwrap();
            std::fs::write(&resolved, bytes).unwrap();
            let output = run_artifact_request(
                tmp.path(),
                "file_read_project_artifact",
                path,
                json!({
                    "path": path,
                    "offset": 0,
                    "length": MAX_MCP_IMAGE_BYTES,
                    "max_file_bytes": MAX_MCP_IMAGE_BYTES,
                    "mcp_image": true,
                }),
            );

            assert_eq!(output["mime_type"], expected_mime);
            assert_eq!(output["file_bytes"], bytes.len());
            assert_eq!(output["bytes_returned"], bytes.len());
            assert_eq!(
                output["content_base64"],
                general_purpose::STANDARD.encode(bytes)
            );
            assert_eq!(output["offset"], 0);
            assert_eq!(output["next_offset"], bytes.len());
            assert_eq!(output["truncated"], false);
            assert_eq!(output["eof"], true);
        }
    }

    #[test]
    fn read_project_artifact_mcp_image_fails_safely() {
        let tmp = tempfile::tempdir().unwrap();
        let unsupported_path = "docs/images/not-an-image.png";
        let unsupported = tmp.path().join(unsupported_path);
        std::fs::create_dir_all(unsupported.parent().unwrap()).unwrap();
        std::fs::write(&unsupported, b"%PDF-1.7\n").unwrap();
        let unsupported_output = run_artifact_request(
            tmp.path(),
            "file_read_project_artifact",
            unsupported_path,
            json!({
                "path": unsupported_path,
                "mcp_image": true,
            }),
        );
        assert!(unsupported_output["error"]
            .as_str()
            .unwrap()
            .contains("unsupported MCP image MIME type 'application/pdf'"));

        let too_large_path = "docs/images/too-large.png";
        let too_large = tmp.path().join(too_large_path);
        let mut oversized_bytes = vec![0u8; MAX_MCP_IMAGE_BYTES + 1];
        oversized_bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        std::fs::write(&too_large, oversized_bytes).unwrap();
        let too_large_output = run_artifact_request(
            tmp.path(),
            "file_read_project_artifact",
            too_large_path,
            json!({
                "path": too_large_path,
                "max_file_bytes": MAX_MCP_IMAGE_BYTES * 2,
                "mcp_image": true,
            }),
        );
        let too_large_error = too_large_output["error"].as_str().unwrap();
        assert!(too_large_error.contains("MCP image too large"));
        assert!(too_large_error.contains(&MAX_MCP_IMAGE_BYTES.to_string()));

        let missing_path = "docs/images/missing.png";
        let missing_output = run_artifact_request(
            tmp.path(),
            "file_read_project_artifact",
            missing_path,
            json!({
                "path": missing_path,
                "mcp_image": true,
            }),
        );
        assert!(missing_output["error"]
            .as_str()
            .unwrap()
            .contains("stat failed"));
    }

    #[test]
    fn artifact_upload_abort_reports_cleanup_and_no_final_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = "artifacts/smoke/abort.artifact";
        let upload_id = "wc_upload_test_abort";
        let resolved = tmp.path().join(path);
        let parent = resolved.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        let (part, sidecar) = upload_paths(parent, upload_id);
        std::fs::write(&part, b"partial").unwrap();
        write_upload_state(
            &sidecar,
            &ArtifactUploadState {
                path: path.to_string(),
                expected_bytes: None,
                expected_sha256: None,
                mime_type: Some("application/octet-stream".to_string()),
                overwrite: false,
                max_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
            },
        )
        .unwrap();

        let output = run_artifact_request(
            tmp.path(),
            "file_artifact_upload_abort",
            path,
            json!({"path": path, "upload_id": upload_id}),
        );

        assert_eq!(output["aborted"], true);
        assert_eq!(output["temp_file_removed"], true);
        assert_eq!(output["sidecar_removed"], true);
        assert_eq!(output["final_file_touched"], false);
        assert_eq!(output["final_file_exists"], false);
        assert_eq!(
            output["changed_path_details"][0]["status"],
            "upload_aborted_no_final_file"
        );
        assert!(!part.exists());
        assert!(!sidecar.exists());
        assert!(!resolved.exists());
    }

    #[test]
    fn artifact_upload_abort_preserves_preexisting_final_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = "artifacts/smoke/preexisting.artifact";
        let upload_id = "wc_upload_test_preexisting";
        let resolved = tmp.path().join(path);
        let parent = resolved.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        std::fs::write(&resolved, b"keep").unwrap();
        let (part, sidecar) = upload_paths(parent, upload_id);
        std::fs::write(&part, b"partial").unwrap();
        write_upload_state(
            &sidecar,
            &ArtifactUploadState {
                path: path.to_string(),
                expected_bytes: None,
                expected_sha256: None,
                mime_type: None,
                overwrite: true,
                max_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
            },
        )
        .unwrap();

        let output = run_artifact_request(
            tmp.path(),
            "file_artifact_upload_abort",
            path,
            json!({"path": path, "upload_id": upload_id}),
        );

        assert_eq!(output["final_file_touched"], false);
        assert_eq!(output["final_file_exists"], true);
        assert_eq!(
            output["changed_path_details"][0]["status"],
            "upload_aborted_final_file_preexisting"
        );
        assert_eq!(std::fs::read(&resolved).unwrap(), b"keep");
    }

    #[test]
    fn artifact_upload_begin_octet_stream_error_is_actionable() {
        let tmp = tempfile::tempdir().unwrap();
        let path = "artifacts/smoke/raw.bin";

        let output = run_artifact_request(
            tmp.path(),
            "file_artifact_upload_begin",
            path,
            json!({
                "path": path,
                "mime_type": "application/octet-stream",
                "max_bytes": DEFAULT_MAX_ARTIFACT_BYTES,
            }),
        );

        let error = output["error"].as_str().unwrap();
        assert_eq!(output["failure_kind"], "policy_rejected");
        assert!(error.contains(".artifact"), "{error}");
        assert!(error.contains(".txt"), "{error}");
        assert!(error.contains("artifacts/smoke/<name>.artifact"), "{error}");
    }

    #[test]
    fn artifact_upload_begin_octet_stream_safe_extension_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let path = "artifacts/smoke/raw.artifact";

        let output = run_artifact_request(
            tmp.path(),
            "file_artifact_upload_begin",
            path,
            json!({
                "path": path,
                "mime_type": "application/octet-stream",
                "max_bytes": DEFAULT_MAX_ARTIFACT_BYTES,
            }),
        );

        assert!(output["error"].is_null() || output.get("error").is_none());
        assert_eq!(output["path"], path);
        assert_eq!(output["committed"], false);
        assert!(output["upload_id"]
            .as_str()
            .unwrap()
            .starts_with("wc_upload_"));
    }
}
