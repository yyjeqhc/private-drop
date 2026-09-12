use super::config::RunnerPolicy;
use super::files::{resolve_requested_path, sha256_hex_bytes};
use super::output::{line_edit_stdout, CommandResult};
use serde::Deserialize;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;
use webcodex_core::runner_operation::RunnerFilePayload;

#[cfg(test)]
pub(crate) fn is_structured_edit_request_kind(kind: &str) -> bool {
    matches!(
        kind,
        "file_write_project_file" | "file_apply_text_edits" | "file_apply_patch"
    )
}

pub(crate) fn validate_structured_edit_runner_path(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("path cannot be empty".to_string());
    }
    if path.contains('\0') {
        return Err("path cannot contain NUL bytes".to_string());
    }
    let raw = Path::new(path);
    if raw.is_absolute() {
        return Err("structured edit path must be project-relative".to_string());
    }
    for component in raw.components() {
        match component {
            std::path::Component::Normal(_) => {}
            _ => return Err("structured edit path must not escape the project".to_string()),
        }
    }
    if is_sensitive_edit_path(path) {
        return Err("refusing to edit sensitive path".to_string());
    }
    Ok(())
}

fn checked_structured_edit_target(cwd: Option<&str>, resolved: &Path) -> Result<PathBuf, String> {
    let root = cwd.ok_or_else(|| "structured edit request missing project root".to_string())?;
    let root = std::fs::canonicalize(root)
        .map_err(|error| format!("project root does not exist: {error}"))?;
    let parent = resolved
        .parent()
        .ok_or_else(|| "target path has no parent directory".to_string())?;
    let name = resolved
        .file_name()
        .ok_or_else(|| "target path has no file name".to_string())?;
    // Resolve parent aliases before planning or creating directories, including
    // any not-yet-created suffix. Keep the final component so batch operations
    // retain their existing rejection of symlink files.
    let target = canonical_batch_identity(parent)?.join(name);
    let identity = canonical_batch_identity(&target)?;
    // Check both the directory entry that will be changed and the target that
    // a content/hash read would follow if the final component is a symlink.
    for candidate in [&target, &identity] {
        let relative = candidate
            .strip_prefix(&root)
            .map_err(|_| "structured edit path escapes project root".to_string())?;
        if is_sensitive_edit_path(&relative.to_string_lossy()) {
            return Err("refusing to edit sensitive path".to_string());
        }
    }
    Ok(target)
}

fn write_file_atomic_strict(path: &Path, content: &str, tmp_prefix: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "target path has no parent directory".to_string())?;
    let original_permissions = std::fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let mut last_error = None;
    for attempt in 0..16 {
        let tmp = parent.join(format!("{tmp_prefix}-{}-{}", std::process::id(), attempt));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(content.as_bytes()) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e.to_string());
                }
                if let Err(e) = file.sync_all() {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e.to_string());
                }
                drop(file);
                if let Some(permissions) = original_permissions.clone() {
                    if let Err(e) = std::fs::set_permissions(&tmp, permissions) {
                        let _ = std::fs::remove_file(&tmp);
                        return Err(e.to_string());
                    }
                }
                if let Err(e) = std::fs::rename(&tmp, path) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e.to_string());
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
    Err(last_error.unwrap_or_else(|| "could not create temporary file".to_string()))
}

fn write_file_atomic(path: &Path, content: &str) -> Result<(), String> {
    write_file_atomic_strict(path, content, ".pd-line")
}

fn write_project_file_atomic(
    path: &Path,
    content: &str,
    overwrite_existing: bool,
) -> Result<(), String> {
    if overwrite_existing {
        write_file_atomic_strict(path, content, ".pd-write")
    } else {
        write_new_file_atomic(path, content)
    }
}

fn parse_json_payload(request: &RunnerFilePayload) -> Result<serde_json::Value, String> {
    serde_json::from_str(request.content.as_deref().unwrap_or_default())
        .map_err(|e| format!("invalid json: {}", e))
}

fn parse_bool_field(payload: &serde_json::Value, key: &str) -> Result<bool, String> {
    match payload.get(key) {
        None | Some(serde_json::Value::Null) => Ok(false),
        Some(serde_json::Value::Bool(v)) => Ok(*v),
        Some(_) => Err(format!("{key} must be a boolean")),
    }
}

fn write_project_file_effect_error(
    path: serde_json::Value,
    error: String,
    state_changed: bool,
    execution_state: &'static str,
) -> serde_json::Value {
    serde_json::json!({
        "path": path,
        "created": false,
        "overwritten": false,
        "bytes_written": 0,
        "sha256": serde_json::Value::Null,
        "changed": false,
        "state_changed": state_changed,
        "execution_state": execution_state,
        "error": error,
    })
}

fn write_project_file_error(path: serde_json::Value, error: String) -> serde_json::Value {
    write_project_file_effect_error(path, error, false, "not_started")
}

fn apply_write_project_file_change(
    resolved: &Path,
    content: &str,
    writer: impl FnOnce(&Path, &str) -> Result<(), String>,
) -> Result<(), ApplyChangeFailure> {
    let created_dirs = create_parent_dirs(resolved)?;
    if let Err(error) = writer(resolved, content) {
        let rollback_complete = cleanup_created_dirs(&created_dirs);
        return Err(ApplyChangeFailure::new(error, rollback_complete));
    }
    Ok(())
}

fn write_project_file_apply_error(
    path: serde_json::Value,
    failure: ApplyChangeFailure,
) -> serde_json::Value {
    let state_changed = !failure.rollback_complete;
    let execution_state = if state_changed {
        "outcome_unknown"
    } else {
        "completed"
    };
    write_project_file_effect_error(
        path,
        format!("write failed: {}", failure.message),
        state_changed,
        execution_state,
    )
}

pub(crate) fn handle_write_project_file_request(
    request: &RunnerFilePayload,
    resolved: &Path,
    start: Instant,
) -> CommandResult {
    let path = request.path.as_str();
    let payload = match parse_json_payload(request) {
        Ok(payload) => payload,
        Err(e) => {
            return line_edit_stdout(write_project_file_error(serde_json::Value::Null, e), start);
        }
    };

    if payload.get("expected_content_prefix").is_some() {
        return line_edit_stdout(
            write_project_file_error(
                serde_json::json!(path),
                "expected_content_prefix is no longer supported; use expected_sha256".to_string(),
            ),
            start,
        );
    }
    let content = match payload.get("content").and_then(serde_json::Value::as_str) {
        Some(value) => value,
        None => {
            return line_edit_stdout(
                write_project_file_error(
                    serde_json::json!(path),
                    "content must be a UTF-8 string without NUL".to_string(),
                ),
                start,
            );
        }
    };
    if content.contains('\0') {
        return line_edit_stdout(
            write_project_file_error(
                serde_json::json!(path),
                "content must be a UTF-8 string without NUL".to_string(),
            ),
            start,
        );
    }

    let overwrite = match parse_bool_field(&payload, "overwrite") {
        Ok(value) => value,
        Err(e) => {
            return line_edit_stdout(write_project_file_error(serde_json::json!(path), e), start);
        }
    };
    let expected_sha = match payload.get("expected_sha256") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(value)) if is_hex_sha256(value) => Some(value.as_str()),
        Some(_) => {
            return line_edit_stdout(
                write_project_file_error(
                    serde_json::json!(path),
                    "expected_sha256 must be a lowercase 64-character hex digest".to_string(),
                ),
                start,
            )
        }
    };
    let target = match checked_structured_edit_target(request.cwd.as_deref(), resolved) {
        Ok(target) => target,
        Err(error) => {
            return line_edit_stdout(
                write_project_file_error(serde_json::json!(path), error),
                start,
            )
        }
    };
    let resolved = target.as_path();
    let exists = std::fs::symlink_metadata(resolved).is_ok();
    if exists && !overwrite {
        return line_edit_stdout(
            write_project_file_error(
                serde_json::json!(path),
                "file exists and overwrite is false".to_string(),
            ),
            start,
        );
    }
    if !exists && (overwrite || expected_sha.is_some()) {
        return line_edit_stdout(
            write_project_file_error(
                serde_json::json!(path),
                "overwrite and expected_sha256 require an existing file; omit both to create"
                    .to_string(),
            ),
            start,
        );
    }

    let current = if exists {
        let Some(expected_sha) = expected_sha else {
            return line_edit_stdout(
                write_project_file_error(
                    serde_json::json!(path),
                    "overwrite requires expected_sha256".to_string(),
                ),
                start,
            );
        };
        let current = match std::fs::read(resolved) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(content) => content,
                Err(_) => {
                    return line_edit_stdout(
                        write_project_file_error(
                            serde_json::json!(path),
                            "existing file is not valid UTF-8".to_string(),
                        ),
                        start,
                    );
                }
            },
            Err(e) => {
                return line_edit_stdout(
                    write_project_file_error(
                        serde_json::json!(path),
                        format!("read failed: {}", e),
                    ),
                    start,
                );
            }
        };
        let current_sha = sha256_hex_bytes(current.as_bytes());
        if expected_sha != current_sha {
            let mut out = write_project_file_error(
                serde_json::json!(path),
                "expected_sha256 mismatch".to_string(),
            );
            out["sha256"] = serde_json::json!(current_sha);
            return line_edit_stdout(out, start);
        }
        Some(current)
    } else {
        None
    };

    let changed = current.as_deref() != Some(content);
    if changed {
        if let Err(failure) = apply_write_project_file_change(resolved, content, |path, content| {
            write_project_file_atomic(path, content, exists)
        }) {
            return line_edit_stdout(
                write_project_file_apply_error(serde_json::json!(path), failure),
                start,
            );
        }
    }
    line_edit_stdout(
        serde_json::json!({
            "path": path,
            "created": !exists,
            "overwritten": exists,
            "bytes_written": if changed { content.len() } else { 0 },
            "sha256": sha256_hex_bytes(content.as_bytes()),
            "changed": changed,
            "state_changed": changed,
            "execution_state": "completed",
        }),
        start,
    )
}

/// Maximum file size accepted by `file_apply_text_edits` on the Runner side.
/// Host-only limit has no twin here; this per-file cap stays Runner-local.
const APPLY_TEXT_EDITS_MAX_FILE_BYTES: usize = 2 * 1024 * 1024; // 2 MiB

// The edit wire types, batch/edit limits, and the sensitive-path guard are
// shared verbatim with the host write path via `apply_edits_shared`; use the
// neutral shared type names directly rather than preserving Runner-local aliases.
use crate::apply_edits_shared::{
    canonicalize_apply_text_line_endings, detect_apply_text_line_ending,
    is_lowercase_hex_sha256 as is_hex_sha256, is_sensitive_edit_path, resolve_apply_text_match,
    restore_apply_text_line_endings, ApplyFileChangeInput, ApplyFileChangeKind, ApplyTextEditInput,
    ApplyTextEditKind, ApplyTextMatchConflict, ApplyTextMatchConflictKind,
    MAX_APPLY_FILE_CHANGES as APPLY_TEXT_EDITS_MAX_CHANGES,
    MAX_APPLY_TEXT_EDITS as APPLY_TEXT_EDITS_MAX_EDITS,
    MAX_APPLY_TEXT_EDIT_FIELD_BYTES as APPLY_TEXT_EDITS_MAX_FIELD_BYTES,
};
use crate::apply_patch_shared::{
    derive_codex_patch_update_with_matching_mode, parse_codex_patch, ApplyPatchMatchingMode,
    CodexPatchChunkMatch, CodexPatchError, CodexPatchHunk, CodexPatchMatchDiagnostic,
};

#[derive(Debug, Deserialize)]
struct ApplyTextEditsPayload {
    changes: Vec<ApplyFileChangeInput>,
    #[serde(default)]
    dry_run: Option<bool>,
    #[serde(default)]
    recovery_metadata_version: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyPatchPayload {
    patch: String,
    #[serde(default)]
    dry_run: Option<bool>,
    #[serde(default)]
    matching_mode: Option<ApplyPatchMatchingMode>,
    /// Rolling-wire compatibility for older Servers only. Current model-facing
    /// requests use matching_mode and never emit this field.
    #[serde(default)]
    strict_matching: Option<bool>,
}

fn apply_patch_matching_mode(
    payload: &ApplyPatchPayload,
) -> Result<ApplyPatchMatchingMode, String> {
    if payload.matching_mode.is_some() && payload.strict_matching.is_some() {
        return Err("matching_mode and legacy strict_matching cannot be combined".to_string());
    }
    Ok(match (payload.matching_mode, payload.strict_matching) {
        (Some(mode), None) => mode,
        (None, Some(true)) => ApplyPatchMatchingMode::ExactUnique,
        // Preserve the old Server wire default when rolling a new Runner first.
        (None, Some(false) | None) => ApplyPatchMatchingMode::FirstMatch,
        (Some(_), Some(_)) => unreachable!(),
    })
}

#[derive(Debug)]
enum EditPlanConflict {
    Match(ApplyTextMatchConflict),
    Overlap {
        first_edit_index: usize,
        second_edit_index: usize,
    },
}

#[derive(Debug)]
struct EditPlanError {
    edit_index: usize,
    edit_kind: &'static str,
    message: String,
    conflict: Option<EditPlanConflict>,
}

impl EditPlanError {
    fn plain(edit_index: usize, edit_kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            edit_index,
            edit_kind,
            message: message.into(),
            conflict: None,
        }
    }
}

struct PlannedFileChange {
    index: usize,
    kind: ApplyFileChangeKind,
    path: String,
    to_path: Option<String>,
    resolved: PathBuf,
    resolved_to: Option<PathBuf>,
    original: Option<String>,
    replacement: Option<String>,
    permissions: Option<std::fs::Permissions>,
    old_sha256: Option<String>,
    new_sha256: Option<String>,
    edit_summaries: Vec<serde_json::Value>,
    would_change: bool,
}

struct AppliedFileChange {
    plan_index: usize,
    created_dirs: Vec<PathBuf>,
}

fn edit_plan(
    original: &str,
    edits: &[ApplyTextEditInput],
) -> Result<(String, Vec<serde_json::Value>), EditPlanError> {
    if edits.is_empty() || edits.len() > APPLY_TEXT_EDITS_MAX_EDITS {
        return Err(EditPlanError::plain(
            0,
            "edit",
            format!(
                "edits must contain 1..={} entries",
                APPLY_TEXT_EDITS_MAX_EDITS
            ),
        ));
    }
    let line_ending = detect_apply_text_line_ending(original)
        .map_err(|error| EditPlanError::plain(0, "edit", error))?;
    let canonical_original = canonicalize_apply_text_line_endings(original, line_ending)
        .map_err(|error| EditPlanError::plain(0, "edit", error))?;
    let original = canonical_original.as_ref();
    let mut ops: Vec<(usize, usize, String, usize)> = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let kind = &edit.kind;
        if edit.occurrence == Some(0) {
            return Err(EditPlanError::plain(
                index,
                kind.as_str(),
                "occurrence must be at least 1",
            ));
        }
        if let Some(line_scope) = edit.line_scope {
            line_scope
                .validate()
                .map_err(|reason| EditPlanError::plain(index, kind.as_str(), reason))?;
        }
        let (needle, replacement): (&str, String) = match kind {
            ApplyTextEditKind::ReplaceExact => {
                let old = edit
                    .old_text
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        EditPlanError::plain(index, kind.as_str(), "old_text must be non-empty")
                    })?;
                if edit.anchor_text.is_some() {
                    return Err(EditPlanError::plain(
                        index,
                        kind.as_str(),
                        "anchor_text is not allowed",
                    ));
                }
                (old, edit.new_text.clone().unwrap_or_default())
            }
            ApplyTextEditKind::DeleteExact => {
                let old = edit
                    .old_text
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        EditPlanError::plain(index, kind.as_str(), "old_text must be non-empty")
                    })?;
                if edit.new_text.is_some() || edit.anchor_text.is_some() {
                    return Err(EditPlanError::plain(
                        index,
                        kind.as_str(),
                        "new_text and anchor_text are not allowed",
                    ));
                }
                (old, String::new())
            }
            ApplyTextEditKind::InsertBefore | ApplyTextEditKind::InsertAfter => {
                let anchor = edit
                    .anchor_text
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        EditPlanError::plain(index, kind.as_str(), "anchor_text must be non-empty")
                    })?;
                let new_text = edit.new_text.as_deref().ok_or_else(|| {
                    EditPlanError::plain(index, kind.as_str(), "new_text is required")
                })?;
                if edit.old_text.is_some() {
                    return Err(EditPlanError::plain(
                        index,
                        kind.as_str(),
                        "old_text is not allowed",
                    ));
                }
                (anchor, new_text.to_string())
            }
        };
        if needle.contains('\0') || replacement.contains('\0') {
            return Err(EditPlanError::plain(
                index,
                kind.as_str(),
                "edit text cannot contain NUL bytes",
            ));
        }
        if needle.len() > APPLY_TEXT_EDITS_MAX_FIELD_BYTES
            || replacement.len() > APPLY_TEXT_EDITS_MAX_FIELD_BYTES
        {
            return Err(EditPlanError::plain(
                index,
                kind.as_str(),
                "edit field is too large",
            ));
        }
        let needle = canonicalize_apply_text_line_endings(needle, line_ending)
            .map_err(|error| EditPlanError::plain(index, kind.as_str(), error))?;
        let replacement = canonicalize_apply_text_line_endings(&replacement, line_ending)
            .map_err(|error| EditPlanError::plain(index, kind.as_str(), error))?
            .into_owned();
        if matches!(
            kind,
            ApplyTextEditKind::InsertBefore | ApplyTextEditKind::InsertAfter
        ) && replacement.is_empty()
        {
            continue;
        }
        let needle = needle.as_ref();
        let (start, end) =
            resolve_apply_text_match(original, needle, edit.occurrence, edit.line_scope.as_ref())
                .map_err(|conflict| {
                let message = match conflict.kind {
                    ApplyTextMatchConflictKind::MatchNotFound if conflict.line_scope.is_some() => {
                        "match text was not found within line_scope".to_string()
                    }
                    ApplyTextMatchConflictKind::MatchNotFound => {
                        "match text was not found".to_string()
                    }
                    ApplyTextMatchConflictKind::MultipleMatches => format!(
                        "match text matched {} times{}",
                        conflict
                            .line_scope_match_count
                            .unwrap_or(conflict.match_count),
                        if conflict.line_scope.is_some() {
                            " within line_scope"
                        } else {
                            ""
                        }
                    ),
                    ApplyTextMatchConflictKind::OccurrenceOutOfRange => format!(
                        "requested occurrence {} is out of range for {} exact matches",
                        conflict.requested_occurrence.unwrap_or(0),
                        conflict.match_count
                    ),
                    ApplyTextMatchConflictKind::OccurrenceOutsideLineScope => {
                        let scope = conflict
                            .line_scope
                            .expect("outside-scope conflict always carries line_scope");
                        format!(
                            "requested occurrence {} is outside line_scope {}..={}",
                            conflict.requested_occurrence.unwrap_or(0),
                            scope.start_line,
                            scope.end_line
                        )
                    }
                };
                EditPlanError {
                    edit_index: index,
                    edit_kind: kind.as_str(),
                    message,
                    conflict: Some(EditPlanConflict::Match(conflict)),
                }
            })?;
        let (range_start, range_end) = match kind {
            ApplyTextEditKind::InsertBefore => (start, start),
            ApplyTextEditKind::InsertAfter => (end, end),
            _ => (start, end),
        };
        ops.push((range_start, range_end, replacement, index));
    }
    ops.sort_by_key(|&(start, end, _, index)| (start, end, index));
    for pair in ops.windows(2) {
        if pair[1].0 < pair[0].1 {
            return Err(EditPlanError {
                edit_index: pair[1].3,
                edit_kind: edits[pair[1].3].kind.as_str(),
                message: "edits overlap".to_string(),
                conflict: Some(EditPlanConflict::Overlap {
                    first_edit_index: pair[0].3,
                    second_edit_index: pair[1].3,
                }),
            });
        }
    }
    let mut replacement = String::with_capacity(original.len() + 64);
    let mut cursor = 0usize;
    let mut summaries = Vec::with_capacity(ops.len());
    for &(start, end, ref text, index) in &ops {
        replacement.push_str(&original[cursor..start]);
        replacement.push_str(text);
        cursor = end;
        let old_start_line = 1 + original[..start].matches('\n').count();
        let mut old_end_line = 1 + original[..end].matches('\n').count();
        if end > start && original.as_bytes().get(end - 1) == Some(&b'\n') {
            old_end_line = old_end_line.saturating_sub(1).max(old_start_line);
        }
        if end == start {
            old_end_line = old_start_line;
        }
        summaries.push(serde_json::json!({
            "index": index,
            "kind": edits[index].kind.as_str(),
            "old_start_line": old_start_line,
            "old_end_line": old_end_line,
            "new_line_count": if text.is_empty() { 0 } else { text.lines().count() },
        }));
    }
    replacement.push_str(&original[cursor..]);
    let replacement = restore_apply_text_line_endings(replacement, line_ending);
    Ok((replacement, summaries))
}

fn edit_conflict_recovery(error: &EditPlanError) -> Option<serde_json::Value> {
    match error.conflict.as_ref()? {
        EditPlanConflict::Match(conflict) => {
            let scoped = conflict.line_scope.is_some();
            let (selector_supported, recovery_action, direct_retry_safe, reread_required) =
                match conflict.kind {
                    ApplyTextMatchConflictKind::MultipleMatches if scoped => {
                        (true, "narrow_line_scope_or_select_occurrence", true, false)
                    }
                    ApplyTextMatchConflictKind::MultipleMatches => {
                        (true, "select_occurrence_or_refine_match", true, false)
                    }
                    ApplyTextMatchConflictKind::OccurrenceOutOfRange => {
                        (true, "choose_valid_occurrence_or_refine_match", true, false)
                    }
                    ApplyTextMatchConflictKind::OccurrenceOutsideLineScope => {
                        (true, "align_occurrence_with_line_scope", true, false)
                    }
                    ApplyTextMatchConflictKind::MatchNotFound if scoped => (
                        conflict.match_count > 0,
                        "adjust_line_scope_or_refine_match",
                        true,
                        false,
                    ),
                    ApplyTextMatchConflictKind::MatchNotFound => {
                        (false, "reread_or_refine_match", false, true)
                    }
                };
            let mut recovery = serde_json::json!({
                "schema_version": 1,
                "conflict_kind": conflict.kind.as_str(),
                "match_count": conflict.match_count,
                "occurrence_selector_supported": selector_supported,
                "direct_retry_safe": direct_retry_safe,
                "reread_required": reread_required,
                "candidate_ranges": conflict.candidate_ranges,
                "candidates_truncated": conflict.candidates_truncated,
                "recovery_action": recovery_action,
            });
            if let Some(requested) = conflict.requested_occurrence {
                recovery["requested_occurrence"] = serde_json::json!(requested);
            }
            if let Some(line_scope) = conflict.line_scope {
                recovery["line_scope"] = serde_json::json!(line_scope);
            }
            if let Some(line_scope_match_count) = conflict.line_scope_match_count {
                recovery["line_scope_match_count"] = serde_json::json!(line_scope_match_count);
            }
            Some(recovery)
        }
        EditPlanConflict::Overlap {
            first_edit_index,
            second_edit_index,
        } => Some(serde_json::json!({
            "schema_version": 1,
            "conflict_kind": "overlapping_edits",
            "occurrence_selector_supported": false,
            "direct_retry_safe": true,
            "reread_required": false,
            "conflicting_edit_indices": [first_edit_index, second_edit_index],
            "recovery_action": "refine_edit_batch",
        })),
    }
}

fn edit_conflict_retry_guidance(recovery: Option<&serde_json::Value>) -> &'static str {
    match recovery
        .and_then(|value| value.get("recovery_action"))
        .and_then(serde_json::Value::as_str)
    {
        Some("select_occurrence_or_refine_match") => {
            "choose an advertised occurrence or refine the exact match; reuse the same expected_sha256 unless you reread or observe a changed file."
        }
        Some("choose_valid_occurrence_or_refine_match") => {
            "choose a valid advertised occurrence or refine the exact match; reuse the same expected_sha256 unless you reread or observe a changed file."
        }
        Some("narrow_line_scope_or_select_occurrence") => {
            "narrow line_scope or choose an advertised global occurrence that is fully contained by it; reuse the same expected_sha256 unless the file changed."
        }
        Some("adjust_line_scope_or_refine_match") => {
            "adjust line_scope or refine the exact match; reuse the same expected_sha256 unless the file changed."
        }
        Some("align_occurrence_with_line_scope") => {
            "use the intended global occurrence with a line_scope that fully contains it, or correct either fence; reuse the same expected_sha256 unless the file changed."
        }
        Some("reread_or_refine_match") => {
            "for model-generated contextual changes, prefer apply_patch; otherwise reread this file or refine the exact match, then retry apply_text_edits with the newly observed expected_sha256."
        }
        Some("refine_edit_batch") => {
            "refine the edit batch so exact edit ranges no longer overlap; reuse the same expected_sha256 unless you reread or observe a changed file."
        }
        _ => "read this file again and use an exact unique anchor.",
    }
}

fn sha256_conflict_recovery(
    expected_sha256: Option<&str>,
    current_sha256: &str,
) -> serde_json::Value {
    let mut recovery = serde_json::json!({
        "schema_version": 1,
        "conflict_kind": "sha256_mismatch",
        "occurrence_selector_supported": false,
        "direct_retry_safe": false,
        "reread_required": true,
        "current_sha256": current_sha256,
        "recovery_action": "reread_file",
    });
    if let Some(expected_sha256) = expected_sha256 {
        recovery["expected_sha256"] = serde_json::json!(expected_sha256);
    }
    recovery
}

fn batch_error(
    change_index: Option<usize>,
    kind: Option<&str>,
    path: Option<&str>,
    code: &str,
    message: impl Into<String>,
    start: Instant,
) -> CommandResult {
    line_edit_stdout(
        serde_json::json!({
            "changed": false,
            "error_kind": code,
            "state_changed": false,
            "change_index": change_index,
            "kind": kind,
            "path": path,
            "error": format!(
                "Rejected transactional file batch: {}. No files were modified. Retry guidance: refresh file hashes/content, correct the failing change, and retry the whole batch.",
                message.into()
            ),
        }),
        start,
    )
}

fn read_batch_file(path: &Path) -> Result<(String, std::fs::Permissions, String), String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "file not found".to_string()
        } else {
            format!("metadata failed: {error}")
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("path must be a regular non-symlink file".to_string());
    }
    if metadata.len() as usize > APPLY_TEXT_EDITS_MAX_FILE_BYTES {
        return Err(format!(
            "file exceeds {} bytes",
            APPLY_TEXT_EDITS_MAX_FILE_BYTES
        ));
    }
    let bytes = std::fs::read(path).map_err(|error| format!("read failed: {error}"))?;
    let sha256 = sha256_hex_bytes(&bytes);
    let content = String::from_utf8(bytes).map_err(|_| "file is not valid UTF-8".to_string())?;
    if content.contains('\0') {
        return Err("file contains NUL bytes".to_string());
    }
    Ok((content, metadata.permissions(), sha256))
}

fn require_batch_path_absent(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err("target already exists".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("target metadata failed: {error}")),
    }
}

fn canonical_batch_identity(path: &Path) -> Result<PathBuf, String> {
    let mut suffix = Vec::new();
    let mut cursor = path;
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(_) => {
                let mut identity = std::fs::canonicalize(cursor)
                    .map_err(|error| format!("path canonicalization failed: {error}"))?;
                for component in suffix.iter().rev() {
                    identity.push(component);
                }
                return Ok(identity);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = cursor
                    .file_name()
                    .ok_or_else(|| "path has no existing ancestor".to_string())?;
                suffix.push(component.to_os_string());
                cursor = cursor
                    .parent()
                    .ok_or_else(|| "path has no existing ancestor".to_string())?;
            }
            Err(error) => return Err(format!("path metadata failed: {error}")),
        }
    }
}

#[derive(Debug)]
struct ApplyChangeFailure {
    message: String,
    rollback_complete: bool,
}

impl ApplyChangeFailure {
    fn new(message: impl Into<String>, rollback_complete: bool) -> Self {
        Self {
            message: message.into(),
            rollback_complete,
        }
    }
}

impl From<String> for ApplyChangeFailure {
    fn from(message: String) -> Self {
        Self::new(message, true)
    }
}

fn cleanup_created_dirs(created_dirs: &[PathBuf]) -> bool {
    let mut complete = true;
    for directory in created_dirs {
        if let Err(error) = std::fs::remove_dir(directory) {
            if error.kind() != std::io::ErrorKind::NotFound {
                complete = false;
            }
        }
    }
    complete
}

fn create_parent_dirs(path: &Path) -> Result<Vec<PathBuf>, ApplyChangeFailure> {
    let parent = path
        .parent()
        .ok_or_else(|| "target path has no parent directory".to_string())?;
    let mut missing = Vec::new();
    let mut cursor = parent;
    while !cursor.exists() {
        missing.push(cursor.to_path_buf());
        cursor = cursor
            .parent()
            .ok_or_else(|| "target parent has no existing ancestor".to_string())?;
    }
    let mut created = Vec::new();
    for directory in missing.iter().rev() {
        if let Err(error) = std::fs::create_dir(directory) {
            created.reverse();
            let rollback_complete = cleanup_created_dirs(&created);
            return Err(ApplyChangeFailure::new(
                error.to_string(),
                rollback_complete,
            ));
        }
        created.push(directory.clone());
    }
    Ok(missing)
}

fn write_new_file_atomic(path: &Path, content: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "target path has no parent directory".to_string())?;
    for attempt in 0..16 {
        let temporary = parent.join(format!(".pd-batch-new-{}-{attempt}", std::process::id()));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        };
        if let Err(error) = file
            .write_all(content.as_bytes())
            .and_then(|_| file.sync_all())
        {
            let _ = std::fs::remove_file(&temporary);
            return Err(error.to_string());
        }
        drop(file);
        match std::fs::hard_link(&temporary, path) {
            Ok(()) => {
                let _ = std::fs::remove_file(&temporary);
                if let Ok(dir) = std::fs::File::open(parent) {
                    let _ = dir.sync_all();
                }
                return Ok(());
            }
            Err(error) => {
                let _ = std::fs::remove_file(&temporary);
                return Err(error.to_string());
            }
        }
    }
    Err("could not allocate a temporary batch file".to_string())
}

fn rollback_change(plan: &PlannedFileChange) -> Result<(), String> {
    match plan.kind {
        ApplyFileChangeKind::Edit => {
            if plan.would_change {
                write_file_atomic(&plan.resolved, plan.original.as_deref().unwrap_or_default())?;
            }
        }
        ApplyFileChangeKind::Create => {
            if plan.resolved.exists() {
                std::fs::remove_file(&plan.resolved).map_err(|error| error.to_string())?;
            }
        }
        ApplyFileChangeKind::Delete => {
            write_new_file_atomic(&plan.resolved, plan.original.as_deref().unwrap_or_default())?;
            if let Some(permissions) = plan.permissions.clone() {
                std::fs::set_permissions(&plan.resolved, permissions)
                    .map_err(|error| error.to_string())?;
            }
        }
        ApplyFileChangeKind::Rename => {
            let destination = plan
                .resolved_to
                .as_deref()
                .ok_or_else(|| "rename rollback missing destination".to_string())?;
            if plan.replacement.is_some() {
                write_new_file_atomic(
                    &plan.resolved,
                    plan.original.as_deref().unwrap_or_default(),
                )?;
                if let Some(permissions) = plan.permissions.clone() {
                    std::fs::set_permissions(&plan.resolved, permissions)
                        .map_err(|error| error.to_string())?;
                }
                std::fs::remove_file(destination).map_err(|error| error.to_string())?;
            } else if destination.exists() && !plan.resolved.exists() {
                std::fs::hard_link(destination, &plan.resolved)
                    .map_err(|error| error.to_string())?;
                std::fs::remove_file(destination).map_err(|error| error.to_string())?;
            }
        }
    }
    Ok(())
}

fn require_planned_source_unchanged(plan: &PlannedFileChange) -> Result<(), ApplyChangeFailure> {
    if matches!(plan.kind, ApplyFileChangeKind::Create) {
        return Ok(());
    }
    let (_, _, current_sha256) = read_batch_file(&plan.resolved)?;
    if plan.old_sha256.as_deref() != Some(current_sha256.as_str()) {
        return Err(ApplyChangeFailure::new(
            "source changed after batch preflight",
            true,
        ));
    }
    Ok(())
}

fn apply_change(plan: &PlannedFileChange) -> Result<Vec<PathBuf>, ApplyChangeFailure> {
    require_planned_source_unchanged(plan)?;
    match plan.kind {
        ApplyFileChangeKind::Edit => {
            if plan.would_change {
                write_file_atomic(
                    &plan.resolved,
                    plan.replacement.as_deref().unwrap_or_default(),
                )?;
            }
            Ok(Vec::new())
        }
        ApplyFileChangeKind::Create => {
            let created_dirs = create_parent_dirs(&plan.resolved)?;
            if let Err(error) = write_new_file_atomic(
                &plan.resolved,
                plan.replacement.as_deref().unwrap_or_default(),
            ) {
                let rollback_complete = cleanup_created_dirs(&created_dirs);
                return Err(ApplyChangeFailure::new(error, rollback_complete));
            }
            Ok(created_dirs)
        }
        ApplyFileChangeKind::Delete => {
            std::fs::remove_file(&plan.resolved).map_err(|error| error.to_string())?;
            Ok(Vec::new())
        }
        ApplyFileChangeKind::Rename => {
            let destination = plan
                .resolved_to
                .as_deref()
                .ok_or_else(|| "rename destination missing".to_string())?;
            let created_dirs = create_parent_dirs(destination)?;
            if let Some(replacement) = plan.replacement.as_deref() {
                if let Err(error) = write_new_file_atomic(destination, replacement) {
                    let rollback_complete = cleanup_created_dirs(&created_dirs);
                    return Err(ApplyChangeFailure::new(error, rollback_complete));
                }
                if let Some(permissions) = plan.permissions.clone() {
                    if let Err(error) = std::fs::set_permissions(destination, permissions) {
                        let destination_cleanup = std::fs::remove_file(destination);
                        let directories_cleaned = cleanup_created_dirs(&created_dirs);
                        return Err(ApplyChangeFailure::new(
                            error.to_string(),
                            destination_cleanup.is_ok() && directories_cleaned,
                        ));
                    }
                }
                if let Err(error) = require_planned_source_unchanged(plan) {
                    let destination_cleanup = std::fs::remove_file(destination);
                    let directories_cleaned = cleanup_created_dirs(&created_dirs);
                    return Err(ApplyChangeFailure::new(
                        error.message,
                        error.rollback_complete
                            && destination_cleanup.is_ok()
                            && directories_cleaned,
                    ));
                }
            } else if let Err(error) = std::fs::hard_link(&plan.resolved, destination) {
                let rollback_complete = cleanup_created_dirs(&created_dirs);
                return Err(ApplyChangeFailure::new(
                    error.to_string(),
                    rollback_complete,
                ));
            }
            if let Err(error) = std::fs::remove_file(&plan.resolved) {
                let destination_cleanup = std::fs::remove_file(destination);
                let directories_cleaned = cleanup_created_dirs(&created_dirs);
                let rollback_complete = destination_cleanup.is_ok() && directories_cleaned;
                let message = match destination_cleanup {
                    Ok(()) => error.to_string(),
                    Err(cleanup_error) => format!(
                        "failed to remove rename source ({error}); destination cleanup also failed ({cleanup_error})"
                    ),
                };
                return Err(ApplyChangeFailure::new(message, rollback_complete));
            }
            Ok(created_dirs)
        }
    }
}

fn execute_planned_file_changes(
    plans: Vec<PlannedFileChange>,
    dry_run: bool,
    requested_matching_mode: Option<ApplyPatchMatchingMode>,
    ignored_noop_count: usize,
    start: Instant,
) -> CommandResult {
    let mut changed_paths = Vec::new();
    for plan in &plans {
        if !plan.would_change {
            continue;
        }
        changed_paths.push(plan.path.clone());
        if let Some(to_path) = &plan.to_path {
            changed_paths.push(to_path.clone());
        }
    }
    let would_change = plans.iter().any(|plan| plan.would_change);
    if !dry_run {
        let mut applied = Vec::new();
        for (plan_index, plan) in plans.iter().enumerate() {
            if !plan.would_change {
                continue;
            }
            match apply_change(plan) {
                Ok(created_dirs) => applied.push(AppliedFileChange {
                    plan_index,
                    created_dirs,
                }),
                Err(error) => {
                    let mut rollback_complete = error.rollback_complete;
                    for applied_change in applied.iter().rev() {
                        if rollback_change(&plans[applied_change.plan_index]).is_err() {
                            rollback_complete = false;
                        }
                        if !cleanup_created_dirs(&applied_change.created_dirs) {
                            rollback_complete = false;
                        }
                    }
                    return line_edit_stdout(
                        serde_json::json!({
                            "changed": !rollback_complete,
                            "state_changed": !rollback_complete,
                            "execution_state": if rollback_complete { "completed" } else { "outcome_unknown" },
                            "error_kind": "transaction_failed",
                            "change_index": plan.index,
                            "kind": plan.kind.as_str(),
                            "path": plan.path,
                            "rollback_complete": rollback_complete,
                            "recovery_action": if rollback_complete { "retry_after_refresh" } else { "reconcile_worktree" },
                            "error": if rollback_complete {
                                format!("Transactional file batch failed and was rolled back: {}", error.message)
                            } else {
                                format!("Transactional file batch failed and rollback was incomplete: {}", error.message)
                            },
                        }),
                        start,
                    );
                }
            }
        }
    }

    let files = plans
        .iter()
        .map(|plan| {
            serde_json::json!({
                "index": plan.index,
                "kind": plan.kind.as_str(),
                "path": plan.path,
                "to_path": plan.to_path,
                "old_sha256": plan.old_sha256,
                "new_sha256": plan.new_sha256,
                "changed": !dry_run && plan.would_change,
                "would_change": plan.would_change,
                "edits": plan.edit_summaries,
            })
        })
        .collect::<Vec<_>>();
    let mut output = serde_json::json!({
        "dry_run": dry_run,
        "applied_count": plans.len(),
        "ignored_noop_count": ignored_noop_count,
        "changed": !dry_run && would_change,
        "state_changed": !dry_run && would_change,
        "execution_state": "completed",
        "would_change": would_change,
        "files": files,
        "changed_paths": changed_paths,
    });
    if let Some(mode) = requested_matching_mode {
        output["requested_matching_mode"] = serde_json::json!(mode.as_str());
    }
    line_edit_stdout(output, start)
}

fn resolve_unique_patch_path(
    policy: &RunnerPolicy,
    request: &RunnerFilePayload,
    touched: &mut HashSet<PathBuf>,
    index: usize,
    kind: &str,
    path: &str,
    start: Instant,
) -> Result<PathBuf, CommandResult> {
    if let Err(error) = validate_structured_edit_runner_path(path) {
        return Err(batch_error(
            Some(index),
            Some(kind),
            None,
            "invalid_path",
            error,
            start,
        ));
    }
    let resolved = resolve_requested_path(policy, request.cwd.as_deref(), path)
        .and_then(|resolved| checked_structured_edit_target(request.cwd.as_deref(), &resolved))
        .map_err(|error| {
            batch_error(
                Some(index),
                Some(kind),
                None,
                "path_policy_rejected",
                error,
                start,
            )
        })?;
    let identity = canonical_batch_identity(&resolved).map_err(|error| {
        batch_error(
            Some(index),
            Some(kind),
            None,
            "path_policy_rejected",
            error,
            start,
        )
    })?;
    if !touched.insert(identity) {
        return Err(batch_error(
            Some(index),
            Some(kind),
            None,
            "path_overlap",
            "source/destination paths may appear only once after path resolution",
            start,
        ));
    }
    Ok(resolved)
}

fn apply_patch_conflict(
    index: usize,
    path: &str,
    error: &CodexPatchError,
    start: Instant,
) -> CommandResult {
    let mut result = serde_json::json!({
        "changed": false,
        "state_changed": false,
        "execution_state": "not_started",
        "error_kind": error.kind,
        "change_index": index,
        "path": path,
        "recovery_action": "reread_or_regenerate_patch",
        "retry_guidance": "use match_diagnostic to target the stale chunk when present; reread the current file, regenerate the Codex patch against that content, and retry the whole batch",
        "error": format!("Rejected Codex patch before write: {}. No files were modified.", error.message),
    });
    if let Some(diagnostic) = error.match_diagnostic.as_ref() {
        result["match_diagnostic"] = apply_patch_match_diagnostic_json(diagnostic);
    }
    line_edit_stdout(result, start)
}

fn apply_patch_match_diagnostic_json(diagnostic: &CodexPatchMatchDiagnostic) -> serde_json::Value {
    serde_json::json!({
        "chunk_index": diagnostic.chunk_index,
        "match_source": diagnostic.match_source.as_str(),
        "search_start_line": diagnostic.search_start_line,
        "expected_line_count": diagnostic.expected_line_count,
        "available_line_count": diagnostic.available_line_count,
        "closest_start_line": diagnostic.closest_start_line,
        "closest_exact_line_matches": diagnostic.closest_exact_line_matches,
        "closest_trim_end_line_matches": diagnostic.closest_trim_end_line_matches,
        "closest_trim_line_matches": diagnostic.closest_trim_line_matches,
        "first_exact_mismatch_offset": diagnostic.first_exact_mismatch_offset,
    })
}

fn apply_patch_matching_mode_rejection(
    index: usize,
    path: &str,
    matched: &CodexPatchChunkMatch,
    start: Instant,
) -> CommandResult {
    let Some(rejection) = matched.match_rejection.as_ref() else {
        return batch_error(
            Some(index),
            Some("edit"),
            Some(path),
            "invalid_match_metadata",
            "matching-mode rejection was missing its bounded rejection fact",
            start,
        );
    };
    let ambiguous = rejection.candidate_count > 1;
    let retry_guidance = match (rejection.requested_matching_mode, ambiguous) {
        (ApplyPatchMatchingMode::Unique, true) => {
            "add a stable parent/function/test/module anchor or small surrounding context and retry with matching_mode=unique; candidate positions are equal observation targets, never a winner"
        }
        (ApplyPatchMatchingMode::ExactUnique, true) => {
            "expand exact context until every textual positioning decision is exact and unique, then retry with matching_mode=exact_unique"
        }
        (ApplyPatchMatchingMode::ExactUnique, false) => {
            "reread the current source and regenerate exact context, then retry with matching_mode=exact_unique"
        }
        _ => "regenerate the patch with unambiguous context and retry",
    };
    line_edit_stdout(
        serde_json::json!({
            "changed": false,
            "state_changed": false,
            "execution_state": "not_started",
            "error_kind": "matching_mode_rejected",
            "change_index": index,
            "path": path,
            "chunk_index": matched.chunk_index,
            "requested_matching_mode": rejection.requested_matching_mode.as_str(),
            "match_mode": rejection.match_mode.as_str(),
            "match_source": rejection.match_source.as_str(),
            "matched_start_line": (!ambiguous).then_some(rejection.matched_start_line),
            "candidate_count": rejection.candidate_count,
            "candidate_start_lines": rejection.candidate_start_lines,
            "candidate_positions_truncated": rejection.candidate_positions_truncated,
            "search_start_line": rejection.search_start_line,
            "source_line_count": rejection.source_line_count,
            "matching_mode_satisfied": false,
            "recovery_action": "refine_patch_context",
            "retry_guidance": retry_guidance,
            "error": format!(
                "Rejected Codex patch before write: {path} chunk {} did not satisfy matching_mode={}. No files were modified.",
                matched.chunk_index, rejection.requested_matching_mode.as_str()
            ),
        }),
        start,
    )
}

pub(crate) fn handle_apply_patch_file_request(
    policy: &RunnerPolicy,
    request: &RunnerFilePayload,
    start: Instant,
) -> CommandResult {
    let payload: ApplyPatchPayload =
        match serde_json::from_str(request.content.as_deref().unwrap_or_default()) {
            Ok(payload) => payload,
            Err(error) => {
                return batch_error(
                    None,
                    None,
                    None,
                    "invalid_payload",
                    format!("invalid JSON payload: {error}"),
                    start,
                )
            }
        };
    let patch = match parse_codex_patch(&payload.patch) {
        Ok(patch) => patch,
        Err(error) => {
            return line_edit_stdout(
                serde_json::json!({
                    "changed": false,
                    "state_changed": false,
                    "execution_state": "not_started",
                    "error_kind": error.kind,
                    "patch_line": error.line,
                    "recovery_action": "regenerate_patch",
                    "expected_format": "codex_patch",
                    "error": format!("Rejected Codex patch before write: {error}. No files were modified."),
                }),
                start,
            )
        }
    };
    let dry_run = payload.dry_run.unwrap_or(false);
    let matching_mode = match apply_patch_matching_mode(&payload) {
        Ok(mode) => mode,
        Err(error) => return batch_error(None, None, None, "invalid_payload", error, start),
    };
    let mut touched = HashSet::new();
    let mut plans = Vec::with_capacity(patch.hunks.len());

    for (index, hunk) in patch.hunks.iter().enumerate() {
        let kind = hunk.kind();
        let path = hunk.path();
        let resolved = match resolve_unique_patch_path(
            policy,
            request,
            &mut touched,
            index,
            kind,
            path,
            start,
        ) {
            Ok(resolved) => resolved,
            Err(result) => return result,
        };

        let planned = match hunk {
            CodexPatchHunk::AddFile { contents, .. } => {
                if contents.contains('\0') || contents.len() > APPLY_TEXT_EDITS_MAX_FILE_BYTES {
                    return batch_error(
                        Some(index),
                        Some(kind),
                        None,
                        "invalid_content",
                        "new file content contains NUL or exceeds the file-size limit",
                        start,
                    );
                }
                if let Err(error) = require_batch_path_absent(&resolved) {
                    return batch_error(
                        Some(index),
                        Some(kind),
                        Some(path),
                        "path_exists",
                        error,
                        start,
                    );
                }
                PlannedFileChange {
                    index,
                    kind: ApplyFileChangeKind::Create,
                    path: path.to_string(),
                    to_path: None,
                    resolved,
                    resolved_to: None,
                    original: None,
                    replacement: Some(contents.clone()),
                    permissions: None,
                    old_sha256: None,
                    new_sha256: Some(sha256_hex_bytes(contents.as_bytes())),
                    edit_summaries: Vec::new(),
                    would_change: true,
                }
            }
            CodexPatchHunk::DeleteFile { .. } => {
                let (original, permissions, old_sha256) = match read_batch_file(&resolved) {
                    Ok(file) => file,
                    Err(error) => {
                        return batch_error(
                            Some(index),
                            Some(kind),
                            Some(path),
                            "read_failed",
                            error,
                            start,
                        )
                    }
                };
                PlannedFileChange {
                    index,
                    kind: ApplyFileChangeKind::Delete,
                    path: path.to_string(),
                    to_path: None,
                    resolved,
                    resolved_to: None,
                    original: Some(original),
                    replacement: None,
                    permissions: Some(permissions),
                    old_sha256: Some(old_sha256),
                    new_sha256: None,
                    edit_summaries: Vec::new(),
                    would_change: true,
                }
            }
            CodexPatchHunk::UpdateFile {
                move_path, chunks, ..
            } => {
                let (original, permissions, old_sha256) = match read_batch_file(&resolved) {
                    Ok(file) => file,
                    Err(error) => {
                        return batch_error(
                            Some(index),
                            Some(kind),
                            Some(path),
                            "read_failed",
                            error,
                            start,
                        )
                    }
                };
                let (replacement, chunk_matches) = if chunks.is_empty() {
                    (original.clone(), Vec::new())
                } else {
                    match derive_codex_patch_update_with_matching_mode(
                        &original,
                        path,
                        chunks,
                        matching_mode,
                    ) {
                        Ok(update) => {
                            if let Some(matched) = update
                                .chunk_matches
                                .iter()
                                .find(|matched| matched.match_rejection.is_some())
                            {
                                return apply_patch_matching_mode_rejection(
                                    index, path, matched, start,
                                );
                            }
                            (update.content, update.chunk_matches)
                        }
                        Err(error) => return apply_patch_conflict(index, path, &error, start),
                    }
                };
                if replacement.contains('\0') || replacement.len() > APPLY_TEXT_EDITS_MAX_FILE_BYTES
                {
                    return batch_error(
                        Some(index),
                        Some(kind),
                        Some(path),
                        "invalid_content",
                        "updated file content contains NUL or exceeds the file-size limit",
                        start,
                    );
                }
                let edit_summaries = chunks
                    .iter()
                    .zip(chunk_matches.iter())
                    .enumerate()
                    .map(|(chunk_index, (chunk, chunk_match))| {
                        serde_json::json!({
                            "chunk_index": chunk_index,
                            "change_context_present": chunk.change_context.is_some(),
                            "old_line_count": chunk.old_lines.len(),
                            "new_line_count": chunk.new_lines.len(),
                            "end_of_file": chunk.is_end_of_file,
                            "match_mode": chunk_match.match_mode.map(|mode| mode.as_str()),
                            "match_source": chunk_match.match_source.as_str(),
                            "matched_start_line": chunk_match.matched_start_line,
                            "candidate_count": chunk_match.candidate_count,
                            "unique_match": chunk_match.unique_match,
                            "strict_match": chunk_match.strict_match,
                        })
                    })
                    .collect::<Vec<_>>();
                let destination_path = move_path
                    .as_deref()
                    .filter(|destination| *destination != path);
                if let Some(destination_path) = destination_path {
                    let destination = match resolve_unique_patch_path(
                        policy,
                        request,
                        &mut touched,
                        index,
                        kind,
                        destination_path,
                        start,
                    ) {
                        Ok(destination) => destination,
                        Err(result) => return result,
                    };
                    if let Err(error) = require_batch_path_absent(&destination) {
                        return batch_error(
                            Some(index),
                            Some(kind),
                            Some(destination_path),
                            "path_exists",
                            error,
                            start,
                        );
                    }
                    let content_changed = replacement != original;
                    PlannedFileChange {
                        index,
                        kind: ApplyFileChangeKind::Rename,
                        path: path.to_string(),
                        to_path: Some(destination_path.to_string()),
                        resolved,
                        resolved_to: Some(destination),
                        original: Some(original),
                        replacement: content_changed.then_some(replacement.clone()),
                        permissions: Some(permissions),
                        old_sha256: Some(old_sha256),
                        new_sha256: Some(sha256_hex_bytes(replacement.as_bytes())),
                        edit_summaries,
                        would_change: true,
                    }
                } else {
                    let new_sha256 = sha256_hex_bytes(replacement.as_bytes());
                    let would_change = replacement != original;
                    PlannedFileChange {
                        index,
                        kind: ApplyFileChangeKind::Edit,
                        path: path.to_string(),
                        to_path: None,
                        resolved,
                        resolved_to: None,
                        original: Some(original),
                        replacement: Some(replacement),
                        permissions: Some(permissions),
                        old_sha256: Some(old_sha256),
                        new_sha256: Some(new_sha256),
                        edit_summaries,
                        would_change,
                    }
                }
            }
        };
        plans.push(planned);
    }

    execute_planned_file_changes(plans, dry_run, Some(matching_mode), 0, start)
}

pub(crate) fn handle_apply_text_edits_file_request(
    policy: &RunnerPolicy,
    request: &RunnerFilePayload,
    start: Instant,
) -> CommandResult {
    let payload: ApplyTextEditsPayload =
        match serde_json::from_str(request.content.as_deref().unwrap_or_default()) {
            Ok(payload) => payload,
            Err(error) => {
                return batch_error(
                    None,
                    None,
                    None,
                    "invalid_payload",
                    format!("invalid JSON payload: {error}"),
                    start,
                )
            }
        };
    if payload.changes.is_empty() || payload.changes.len() > APPLY_TEXT_EDITS_MAX_CHANGES {
        return batch_error(
            None,
            None,
            None,
            "invalid_change_count",
            format!("changes must contain 1..={APPLY_TEXT_EDITS_MAX_CHANGES} entries"),
            start,
        );
    }
    let dry_run = payload.dry_run.unwrap_or(false);
    let ignored_noop_count = payload
        .changes
        .iter()
        .flat_map(|change| change.edits.iter())
        .filter(|edit| {
            matches!(
                edit.kind,
                ApplyTextEditKind::InsertBefore | ApplyTextEditKind::InsertAfter
            ) && edit.new_text.as_deref() == Some("")
        })
        .count();
    let mut touched = HashSet::new();
    let mut plans = Vec::with_capacity(payload.changes.len());
    for (index, change) in payload.changes.iter().enumerate() {
        if let Err(error) = validate_structured_edit_runner_path(&change.path) {
            return batch_error(
                Some(index),
                Some(change.kind.as_str()),
                Some(&change.path),
                "invalid_path",
                error,
                start,
            );
        }
        let resolved = match resolve_requested_path(policy, request.cwd.as_deref(), &change.path)
            .and_then(|resolved| checked_structured_edit_target(request.cwd.as_deref(), &resolved))
        {
            Ok(path) => path,
            Err(error) => {
                return batch_error(
                    Some(index),
                    Some(change.kind.as_str()),
                    Some(&change.path),
                    "path_policy_rejected",
                    error,
                    start,
                )
            }
        };
        let resolved_identity = match canonical_batch_identity(&resolved) {
            Ok(identity) => identity,
            Err(error) => {
                return batch_error(
                    Some(index),
                    Some(change.kind.as_str()),
                    Some(&change.path),
                    "path_policy_rejected",
                    error,
                    start,
                )
            }
        };
        if !touched.insert(resolved_identity) {
            return batch_error(
                Some(index),
                Some(change.kind.as_str()),
                Some(&change.path),
                "path_overlap",
                "source/destination paths may appear only once after path resolution",
                start,
            );
        }
        let resolved_to = if let Some(to_path) = change.to_path.as_deref() {
            if let Err(error) = validate_structured_edit_runner_path(to_path) {
                return batch_error(
                    Some(index),
                    Some(change.kind.as_str()),
                    Some(to_path),
                    "invalid_path",
                    error,
                    start,
                );
            }
            match resolve_requested_path(policy, request.cwd.as_deref(), to_path).and_then(
                |resolved| checked_structured_edit_target(request.cwd.as_deref(), &resolved),
            ) {
                Ok(path) => {
                    let identity = match canonical_batch_identity(&path) {
                        Ok(identity) => identity,
                        Err(error) => {
                            return batch_error(
                                Some(index),
                                Some(change.kind.as_str()),
                                Some(to_path),
                                "path_policy_rejected",
                                error,
                                start,
                            )
                        }
                    };
                    if !touched.insert(identity) {
                        return batch_error(
                            Some(index),
                            Some(change.kind.as_str()),
                            Some(to_path),
                            "path_overlap",
                            "source/destination paths may appear only once after path resolution",
                            start,
                        );
                    }
                    Some(path)
                }
                Err(error) => {
                    return batch_error(
                        Some(index),
                        Some(change.kind.as_str()),
                        Some(to_path),
                        "path_policy_rejected",
                        error,
                        start,
                    )
                }
            }
        } else {
            None
        };

        let planned = match change.kind {
            ApplyFileChangeKind::Create => {
                if change.to_path.is_some()
                    || change.expected_sha256.is_some()
                    || !change.edits.is_empty()
                {
                    return batch_error(
                        Some(index),
                        Some("create"),
                        Some(&change.path),
                        "invalid_fields",
                        "create allows only path and content",
                        start,
                    );
                }
                let content = match change.content.as_deref() {
                    Some(content)
                        if !content.contains('\0')
                            && content.len() <= APPLY_TEXT_EDITS_MAX_FILE_BYTES =>
                    {
                        content.to_string()
                    }
                    Some(_) => {
                        return batch_error(
                            Some(index),
                            Some("create"),
                            Some(&change.path),
                            "invalid_content",
                            "content contains NUL or exceeds the file-size limit",
                            start,
                        )
                    }
                    None => {
                        return batch_error(
                            Some(index),
                            Some("create"),
                            Some(&change.path),
                            "invalid_fields",
                            "content is required",
                            start,
                        )
                    }
                };
                if let Err(error) = require_batch_path_absent(&resolved) {
                    return batch_error(
                        Some(index),
                        Some("create"),
                        Some(&change.path),
                        "path_exists",
                        error,
                        start,
                    );
                }
                let new_sha256 = sha256_hex_bytes(content.as_bytes());
                PlannedFileChange {
                    index,
                    kind: change.kind,
                    path: change.path.clone(),
                    to_path: None,
                    resolved,
                    resolved_to: None,
                    original: None,
                    replacement: Some(content),
                    permissions: None,
                    old_sha256: None,
                    new_sha256: Some(new_sha256),
                    edit_summaries: Vec::new(),
                    would_change: true,
                }
            }
            ApplyFileChangeKind::Edit
            | ApplyFileChangeKind::Delete
            | ApplyFileChangeKind::Rename => {
                let (original, permissions, old_sha256) = match read_batch_file(&resolved) {
                    Ok(file) => file,
                    Err(error) => {
                        return batch_error(
                            Some(index),
                            Some(change.kind.as_str()),
                            Some(&change.path),
                            "read_failed",
                            error,
                            start,
                        )
                    }
                };
                if change.expected_sha256.as_deref() != Some(old_sha256.as_str()) {
                    let mut result = serde_json::json!({
                        "changed": false,
                        "error_kind": "sha256_conflict",
                        "state_changed": false,
                        "change_index": index,
                        "kind": change.kind.as_str(),
                        "path": change.path,
                        "error": format!(
                            "Rejected transactional file batch: expected_sha256 does not match current sha256 {old_sha256}. No files were modified. Retry guidance: refresh file hashes/content, correct the failing change, and retry the whole batch."
                        ),
                    });
                    if payload.recovery_metadata_version == Some(1) {
                        result["conflict_recovery"] = sha256_conflict_recovery(
                            change.expected_sha256.as_deref(),
                            &old_sha256,
                        );
                        result["retry_guidance"] = serde_json::json!(
                            "reread the file to obtain current content and sha256, then retry the whole batch with refreshed guards"
                        );
                    }
                    return line_edit_stdout(result, start);
                }
                match change.kind {
                    ApplyFileChangeKind::Edit => {
                        if change.to_path.is_some() || change.content.is_some() {
                            return batch_error(
                                Some(index),
                                Some("edit"),
                                Some(&change.path),
                                "invalid_fields",
                                "edit does not allow to_path or content",
                                start,
                            );
                        }
                        let (replacement, summaries) = match edit_plan(&original, &change.edits) {
                            Ok(plan) => plan,
                            Err(error) => {
                                let recovery = if payload.recovery_metadata_version == Some(1) {
                                    edit_conflict_recovery(&error)
                                } else {
                                    None
                                };
                                let retry_guidance =
                                    edit_conflict_retry_guidance(recovery.as_ref());
                                let mut result = serde_json::json!({
                                    "changed": false,
                                    "error_kind": "edit_conflict",
                                    "state_changed": false,
                                    "change_index": index,
                                    "edit_index": error.edit_index,
                                    "kind": error.edit_kind,
                                    "path": change.path,
                                    "retry_guidance": retry_guidance,
                                    "error": format!(
                                        "Rejected transactional file batch: {}. No files were modified. Retry guidance: {retry_guidance}",
                                        error.message
                                    ),
                                });
                                if let Some(recovery) = recovery {
                                    result["conflict_recovery"] = recovery;
                                }
                                return line_edit_stdout(result, start);
                            }
                        };
                        let new_sha256 = sha256_hex_bytes(replacement.as_bytes());
                        let would_change = replacement != original;
                        PlannedFileChange {
                            index,
                            kind: change.kind,
                            path: change.path.clone(),
                            to_path: None,
                            resolved,
                            resolved_to: None,
                            original: Some(original),
                            replacement: Some(replacement),
                            permissions: Some(permissions),
                            old_sha256: Some(old_sha256),
                            new_sha256: Some(new_sha256),
                            edit_summaries: summaries,
                            would_change,
                        }
                    }
                    ApplyFileChangeKind::Delete => {
                        if change.to_path.is_some()
                            || change.content.is_some()
                            || !change.edits.is_empty()
                        {
                            return batch_error(
                                Some(index),
                                Some("delete"),
                                Some(&change.path),
                                "invalid_fields",
                                "delete allows only path and expected_sha256",
                                start,
                            );
                        }
                        PlannedFileChange {
                            index,
                            kind: change.kind,
                            path: change.path.clone(),
                            to_path: None,
                            resolved,
                            resolved_to: None,
                            original: Some(original),
                            replacement: None,
                            permissions: Some(permissions),
                            old_sha256: Some(old_sha256),
                            new_sha256: None,
                            edit_summaries: Vec::new(),
                            would_change: true,
                        }
                    }
                    ApplyFileChangeKind::Rename => {
                        if change.content.is_some() || !change.edits.is_empty() {
                            return batch_error(
                                Some(index),
                                Some("rename"),
                                Some(&change.path),
                                "invalid_fields",
                                "rename allows only path, to_path, and expected_sha256",
                                start,
                            );
                        }
                        let destination = match resolved_to.as_ref() {
                            Some(destination) if destination != &resolved => destination,
                            _ => {
                                return batch_error(
                                    Some(index),
                                    Some("rename"),
                                    Some(&change.path),
                                    "invalid_destination",
                                    "to_path is required and must differ from path",
                                    start,
                                )
                            }
                        };
                        if let Err(error) = require_batch_path_absent(destination) {
                            return batch_error(
                                Some(index),
                                Some("rename"),
                                change.to_path.as_deref(),
                                "path_exists",
                                error,
                                start,
                            );
                        }
                        PlannedFileChange {
                            index,
                            kind: change.kind,
                            path: change.path.clone(),
                            to_path: change.to_path.clone(),
                            resolved,
                            resolved_to,
                            original: Some(original),
                            replacement: None,
                            permissions: Some(permissions),
                            old_sha256: Some(old_sha256.clone()),
                            new_sha256: Some(old_sha256),
                            edit_summaries: Vec::new(),
                            would_change: true,
                        }
                    }
                    ApplyFileChangeKind::Create => unreachable!(),
                }
            }
        };
        plans.push(planned);
    }

    execute_planned_file_changes(plans, dry_run, None, ignored_noop_count, start)
}

#[cfg(test)]
mod write_project_file_effect_tests {
    use super::*;

    #[test]
    fn file_write_project_file_create_commit_preserves_concurrent_file() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("new.txt");
        let existed_at_preflight = std::fs::symlink_metadata(&target).is_ok();
        assert!(!existed_at_preflight);

        // Another writer creates the target after the request's absence check.
        std::fs::write(&target, "concurrent content").unwrap();
        let failure =
            apply_write_project_file_change(&target, "request content", |path, content| {
                write_project_file_atomic(path, content, existed_at_preflight)
            })
            .unwrap_err();

        assert!(failure.rollback_complete);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "concurrent content"
        );
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
        let output = write_project_file_apply_error(serde_json::json!("new.txt"), failure);
        assert_eq!(output["created"], false);
        assert_eq!(output["changed"], false);
        assert_eq!(output["state_changed"], false);
    }

    #[test]
    fn parent_creation_write_failure_reports_rollback_truth() {
        let temp = tempfile::tempdir().unwrap();

        let clean_target = temp.path().join("clean/a/file.txt");
        let clean_failure = apply_write_project_file_change(&clean_target, "content", |_, _| {
            Err("injected write failure".to_string())
        })
        .unwrap_err();
        assert!(clean_failure.rollback_complete);
        assert!(!temp.path().join("clean").exists());
        let clean =
            write_project_file_apply_error(serde_json::json!("clean/a/file.txt"), clean_failure);
        assert_eq!(clean["changed"], false);
        assert_eq!(clean["state_changed"], false);
        assert_eq!(clean["execution_state"], "completed");

        let dirty_target = temp.path().join("dirty/a/file.txt");
        let dirty_failure = apply_write_project_file_change(&dirty_target, "content", |path, _| {
            std::fs::write(path.parent().unwrap().join("leftover"), "effect").unwrap();
            Err("injected write failure".to_string())
        })
        .unwrap_err();
        assert!(!dirty_failure.rollback_complete);
        let dirty =
            write_project_file_apply_error(serde_json::json!("dirty/a/file.txt"), dirty_failure);
        assert_eq!(dirty["changed"], false);
        assert_eq!(dirty["state_changed"], true);
        assert_eq!(dirty["execution_state"], "outcome_unknown");
        assert!(temp.path().join("dirty/a/leftover").exists());
    }
}
