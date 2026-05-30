use crate::projects::canonicalize_and_verify;

use super::security::is_sensitive_path;
use super::types::{EditOperation, EditResponse};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MAX_EDIT_FILE_SIZE: u64 = 2 * 1024 * 1024;

pub(super) fn edit_error(error: String) -> EditResponse {
    EditResponse {
        success: false,
        changed_files: Vec::new(),
        diff: String::new(),
        warnings: Vec::new(),
        error: Some(error),
    }
}

pub(super) fn edit_path(edit: &EditOperation) -> &str {
    match edit {
        EditOperation::ReplaceText { path, .. }
        | EditOperation::ReplaceRange { path, .. }
        | EditOperation::AppendFile { path, .. }
        | EditOperation::CreateFile { path, .. }
        | EditOperation::WriteFile { path, .. }
        | EditOperation::CreateBinaryFile { path, .. }
        | EditOperation::WriteBinaryFile { path, .. }
        | EditOperation::CreateBinaryArtifact { path, .. }
        | EditOperation::WriteBinaryArtifact { path, .. }
        | EditOperation::CreateBinaryFileFromUpload { path, .. }
        | EditOperation::WriteBinaryFileFromUpload { path, .. }
        | EditOperation::CreateBinaryFileFromUrl { path, .. }
        | EditOperation::WriteBinaryFileFromUrl { path, .. } => path,
    }
}

pub(super) fn edit_text_len(edit: &EditOperation) -> usize {
    match edit {
        EditOperation::ReplaceText { new_text, .. } => new_text.len(),
        EditOperation::ReplaceRange { new_text, .. } => new_text.len(),
        EditOperation::AppendFile { text, .. } => text.len(),
        EditOperation::CreateFile { content, .. } => content.len(),
        EditOperation::WriteFile { content, .. } => content.len(),
        EditOperation::CreateBinaryFile { .. }
        | EditOperation::WriteBinaryFile { .. }
        | EditOperation::CreateBinaryArtifact { .. }
        | EditOperation::WriteBinaryArtifact { .. }
        | EditOperation::CreateBinaryFileFromUpload { .. }
        | EditOperation::WriteBinaryFileFromUpload { .. }
        | EditOperation::CreateBinaryFileFromUrl { .. }
        | EditOperation::WriteBinaryFileFromUrl { .. } => 0,
    }
}

fn edit_kind(edit: &EditOperation) -> &'static str {
    match edit {
        EditOperation::ReplaceText { .. }
        | EditOperation::ReplaceRange { .. }
        | EditOperation::AppendFile { .. }
        | EditOperation::CreateFile { .. }
        | EditOperation::WriteFile { .. } => "text",
        EditOperation::CreateBinaryFile { .. }
        | EditOperation::WriteBinaryFile { .. }
        | EditOperation::CreateBinaryArtifact { .. }
        | EditOperation::WriteBinaryArtifact { .. }
        | EditOperation::CreateBinaryFileFromUpload { .. }
        | EditOperation::WriteBinaryFileFromUpload { .. }
        | EditOperation::CreateBinaryFileFromUrl { .. }
        | EditOperation::WriteBinaryFileFromUrl { .. } => "binary",
    }
}

pub(super) fn validate_no_mixed_edit_kinds(edits: &[EditOperation]) -> Result<(), String> {
    let mut kinds: HashMap<&str, &'static str> = HashMap::new();
    for edit in edits {
        let path = edit_path(edit);
        let kind = edit_kind(edit);
        if let Some(previous) = kinds.insert(path, kind) {
            if previous != kind {
                return Err(format!(
                    "cannot mix text and binary edits for the same path: {}",
                    path
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn ensure_parent_dir(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "path has no parent directory".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create parent directory: {}", e))
}

pub(super) fn validate_edit_path(rel_path: &str) -> Result<(), String> {
    if rel_path.is_empty() {
        return Err("path cannot be empty".to_string());
    }
    if rel_path.starts_with('/') {
        return Err("Absolute paths are not allowed".to_string());
    }
    if rel_path.contains("..") {
        return Err("Path traversal (..) is not allowed".to_string());
    }
    if is_sensitive_path(rel_path) {
        return Err(format!("Cannot modify sensitive path: {}", rel_path));
    }
    Ok(())
}

pub(super) fn simple_binary_diff(path: &str, old_len: Option<usize>, new_len: usize) -> String {
    match old_len {
        Some(old_len) => format!(
            "diff --git a/{0} b/{0}\nBinary files a/{0} and b/{0} differ\n# old size: {1} bytes\n# new size: {2} bytes\n",
            path, old_len, new_len
        ),
        None => format!(
            "diff --git a/{0} b/{0}\nnew file mode 100644\nBinary file b/{0} added\n# new size: {1} bytes\n",
            path, new_len
        ),
    }
}

pub(super) fn simple_file_diff(path: &str, old: Option<&str>, new: &str) -> String {
    let mut out = format!("diff --git a/{0} b/{0}\n--- a/{0}\n+++ b/{0}\n", path);
    out.push_str("@@\n");
    if let Some(old) = old {
        for line in old.lines() {
            out.push_str(&format!("-{}\n", line));
        }
    } else {
        out.push_str("--- /dev/null\n");
    }
    for line in new.lines() {
        out.push_str(&format!("+{}\n", line));
    }
    out
}

pub(super) fn resolve_edit_path(
    root: &Path,
    rel_path: &str,
    must_exist: bool,
) -> Result<PathBuf, String> {
    validate_edit_path(rel_path)?;
    let full_path = root.join(rel_path);
    if must_exist {
        return canonicalize_and_verify(&full_path, root);
    }
    let parent = full_path
        .parent()
        .ok_or_else(|| "path has no parent directory".to_string())?;
    let mut ancestor = parent;
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| "path has no existing parent directory".to_string())?;
    }
    canonicalize_and_verify(ancestor, root)?;
    Ok(full_path)
}

pub(super) fn read_edit_file(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("Failed to stat file: {}", e))?;
    if meta.len() > MAX_EDIT_FILE_SIZE {
        return Err(format!(
            "File is too large for edit API: {} bytes",
            meta.len()
        ));
    }
    std::fs::read_to_string(path).map_err(|e| format!("Failed to read UTF-8 text file: {}", e))
}

pub(super) fn replace_nth(
    content: &str,
    old_text: &str,
    new_text: &str,
    occurrence: Option<usize>,
) -> Result<String, String> {
    if old_text.is_empty() {
        return Err("old_text cannot be empty".to_string());
    }
    let matches: Vec<usize> = content
        .match_indices(old_text)
        .map(|(idx, _)| idx)
        .collect();
    if matches.is_empty() {
        return Err("old_text was not found".to_string());
    }
    let selected = match occurrence {
        Some(n) if n == 0 => return Err("occurrence is 1-based and must be >= 1".to_string()),
        Some(n) if n <= matches.len() => matches[n - 1],
        Some(n) => {
            return Err(format!(
                "occurrence {} exceeds match count {}",
                n,
                matches.len()
            ))
        }
        None if matches.len() == 1 => matches[0],
        None => {
            return Err(format!(
                "old_text matched {} times; specify occurrence",
                matches.len()
            ))
        }
    };
    let mut output = String::new();
    output.push_str(&content[..selected]);
    output.push_str(new_text);
    output.push_str(&content[selected + old_text.len()..]);
    Ok(output)
}

pub(super) fn replace_line_range(
    content: &str,
    start_line: usize,
    end_line: usize,
    new_text: &str,
) -> Result<String, String> {
    if start_line == 0 || end_line == 0 || start_line > end_line {
        return Err(
            "start_line and end_line must be 1-based and start_line <= end_line".to_string(),
        );
    }
    let had_trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    if end_line > lines.len() {
        return Err(format!(
            "line range {}-{} exceeds file line count {}",
            start_line,
            end_line,
            lines.len()
        ));
    }
    let replacement: Vec<String> = if new_text.is_empty() {
        Vec::new()
    } else {
        new_text
            .trim_end_matches('\n')
            .lines()
            .map(|l| l.to_string())
            .collect()
    };
    lines.splice(start_line - 1..end_line, replacement);
    let mut output = lines.join("\n");
    if had_trailing_newline || new_text.ends_with('\n') {
        output.push('\n');
    }
    Ok(output)
}

pub(super) fn load_edit_content(
    root: &Path,
    rel_path: &str,
    paths: &mut HashMap<String, PathBuf>,
    originals: &mut HashMap<String, Option<String>>,
    current: &mut HashMap<String, Option<String>>,
) -> Result<String, String> {
    if let Some(Some(content)) = current.get(rel_path) {
        return Ok(content.clone());
    }
    let full_path = resolve_edit_path(root, rel_path, true)?;
    let content = read_edit_file(&full_path)?;
    paths.insert(rel_path.to_string(), full_path);
    originals
        .entry(rel_path.to_string())
        .or_insert_with(|| Some(content.clone()));
    current.insert(rel_path.to_string(), Some(content.clone()));
    Ok(content)
}
