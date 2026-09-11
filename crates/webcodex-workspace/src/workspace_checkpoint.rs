use crate::path_policy::sensitive_path;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const MAX_DIFF_BYTES: usize = 1024 * 1024;
const MAX_STATUS_BYTES: usize = 256 * 1024;
const MAX_DIFF_STAT_BYTES: usize = 64 * 1024;
const MAX_UNTRACKED_BYTES: usize = 256 * 1024;
const MAX_UNTRACKED_TOTAL_BYTES: usize = 1024 * 1024;
const MAX_UNTRACKED_FILES: usize = 64;

#[derive(Debug, Clone)]
struct StatusEntry {
    path: String,
    status: String,
    staged: bool,
    unstaged: bool,
}

#[derive(Debug, Clone)]
struct UntrackedCheckpointFile {
    path: String,
    content: String,
}

pub fn create_workspace_checkpoint(root: &Path, include_untracked: bool) -> Value {
    match create_workspace_checkpoint_inner(root, include_untracked) {
        Ok(value) | Err(value) => value,
    }
}

pub fn restore_workspace_checkpoint(root: &Path, checkpoint: &Value) -> Value {
    match restore_workspace_checkpoint_inner(root, checkpoint) {
        Ok(value) | Err(value) => value,
    }
}

fn create_workspace_checkpoint_inner(root: &Path, include_untracked: bool) -> Result<Value, Value> {
    let root = canonical_project_root(root)?;
    let head = bounded_git_text(&root, &["rev-parse", "HEAD"], "git HEAD", 64 * 1024)?
        .trim()
        .to_string();
    let branch = branch_name(&root)?;
    let status = bounded_git_text(
        &root,
        &["status", "--porcelain=v1", "--untracked-files=all"],
        "git status",
        MAX_STATUS_BYTES,
    )?;
    let (status_summary, status_entries) = status_summary(&status);
    if let Some(entry) = status_entries
        .iter()
        .find(|entry| entry.status != "untracked" && sensitive_path(&entry.path))
    {
        return Err(fail_extra(
            "sensitive_or_invalid_path",
            "checkpoint tracked diff contains a sensitive path",
            vec![("path", json!(entry.path)), ("state_changed", json!(false))],
        ));
    }

    let tracked_diff = bounded_git_text(
        &root,
        &["diff", "--no-ext-diff", "--"],
        "tracked diff",
        MAX_DIFF_BYTES,
    )?;
    let staged_diff = bounded_git_text(
        &root,
        &["diff", "--cached", "--no-ext-diff", "--"],
        "staged diff",
        MAX_DIFF_BYTES,
    )?;
    let diff_stat = bounded_git_text(
        &root,
        &["diff", "--stat", "--"],
        "tracked diff stat",
        MAX_DIFF_STAT_BYTES,
    )?;
    let staged_diff_stat = bounded_git_text(
        &root,
        &["diff", "--cached", "--stat", "--"],
        "staged diff stat",
        MAX_DIFF_STAT_BYTES,
    )?;
    let (untracked_files, skipped_files) = collect_untracked(&root, include_untracked)?;

    Ok(json!({
        "format": "webcodex.workspace_checkpoint.v1",
        "version": 1,
        "head": head,
        "branch": branch,
        "status_porcelain": status,
        "status_summary": status_summary,
        "tracked_diff": tracked_diff,
        "staged_diff": staged_diff,
        "tracked_diff_bytes": tracked_diff.len(),
        "staged_diff_bytes": staged_diff.len(),
        "diff_stat": diff_stat,
        "staged_diff_stat": staged_diff_stat,
        "untracked_files": untracked_files,
        "skipped_files": skipped_files,
        "complete": true,
        "limitations": [
            "text_diffs_only",
            "ignored_files_excluded",
            "large_binary_secret_like_untracked_files_skipped"
        ],
    }))
}

fn restore_workspace_checkpoint_inner(root: &Path, checkpoint: &Value) -> Result<Value, Value> {
    let root = canonical_project_root(root)?;
    let checkpoint_id = checkpoint
        .get("checkpoint_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| fail("invalid_checkpoint", "checkpoint_id and head are required"))?;
    let expected_head = checkpoint
        .get("head")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| fail("invalid_checkpoint", "checkpoint_id and head are required"))?;

    let head = bounded_git_text(&root, &["rev-parse", "HEAD"], "git HEAD", 64 * 1024)?
        .trim()
        .to_string();
    if head != expected_head {
        return Err(fail_extra(
            "head_mismatch",
            "current HEAD does not match checkpoint head",
            vec![
                ("current_head", json!(head)),
                ("checkpoint_head", json!(expected_head)),
            ],
        ));
    }

    let tracked_diff = checkpoint
        .get("tracked_diff")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let staged_diff = checkpoint
        .get("staged_diff")
        .and_then(Value::as_str)
        .unwrap_or_default();
    validate_checkpoint_diff("tracked_diff", tracked_diff)?;
    validate_checkpoint_diff("staged_diff", staged_diff)?;

    let untracked_values = checkpoint
        .get("untracked_files")
        .and_then(Value::as_array)
        .ok_or_else(|| fail("invalid_checkpoint", "untracked_files must be an array"))?;
    let untracked_files = validate_untracked_checkpoint_files(&root, untracked_values)?;

    let current_unstaged = git_text(
        &root,
        &["diff", "--no-ext-diff", "--"],
        "current unstaged diff",
    )?;
    let current_staged = git_text(
        &root,
        &["diff", "--cached", "--no-ext-diff", "--"],
        "current staged diff",
    )?;

    if !current_unstaged.is_empty() {
        git_apply(&root, &["--reverse", "--check"], &current_unstaged).map_err(|detail| {
            fail_extra(
                "unsafe_current_state",
                "current tracked changes cannot be safely reversed",
                vec![("detail", json!(detail))],
            )
        })?;
    }
    if !current_staged.is_empty() {
        git_apply(
            &root,
            &["--reverse", "--cached", "--check"],
            &current_staged,
        )
        .map_err(|detail| {
            fail_extra(
                "unsafe_current_state",
                "current tracked changes cannot be safely reversed",
                vec![("detail", json!(detail))],
            )
        })?;
    }

    let mut applied_checkpoint_staged_index = false;
    let mut applied_checkpoint_staged_worktree = false;
    let mut applied_checkpoint_unstaged = false;
    let mut created_untracked = Vec::new();

    let restore_result = (|| -> Result<(), String> {
        if !current_unstaged.is_empty() {
            git_apply(&root, &["--reverse"], &current_unstaged)?;
        }
        if !current_staged.is_empty() {
            git_apply(&root, &["--reverse", "--cached"], &current_staged)?;
            git_apply(&root, &["--reverse"], &current_staged)?;
        }

        if !staged_diff.is_empty() {
            git_apply(&root, &["--cached", "--check"], staged_diff)?;
            git_apply(&root, &["--cached"], staged_diff)?;
            applied_checkpoint_staged_index = true;
            git_apply(&root, &["--check"], staged_diff)?;
            git_apply(&root, &[], staged_diff)?;
            applied_checkpoint_staged_worktree = true;
        }
        if !tracked_diff.is_empty() {
            git_apply(&root, &["--check"], tracked_diff)?;
            git_apply(&root, &[], tracked_diff)?;
            applied_checkpoint_unstaged = true;
        }

        for item in &untracked_files {
            let full = root.join(&item.path);
            if full.exists() {
                continue;
            }
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent)
                    .map_err(|err| format!("failed to create parent directory: {err}"))?;
            }
            fs::write(&full, item.content.as_bytes())
                .map_err(|err| format!("failed to write untracked file {}: {err}", item.path))?;
            created_untracked.push(full);
        }
        Ok(())
    })();

    if let Err(detail) = restore_result {
        let rollback_ok = rollback_checkpoint(
            &root,
            &created_untracked,
            applied_checkpoint_staged_index,
            applied_checkpoint_staged_worktree,
            applied_checkpoint_unstaged,
            staged_diff,
            tracked_diff,
        )
        .and_then(|_| reapply_current(&root, &current_staged, &current_unstaged))
        .is_ok();
        return Err(fail_extra(
            "restore_failed",
            "checkpoint restore failed",
            vec![
                ("detail", json!(detail)),
                ("rolled_back", json!(rollback_ok)),
            ],
        ));
    }

    let mut changed_paths = Vec::new();
    for path in changed_paths_from_diff(staged_diff)
        .into_iter()
        .chain(changed_paths_from_diff(tracked_diff))
    {
        push_unique(&mut changed_paths, &path);
    }
    for item in &untracked_files {
        push_unique(&mut changed_paths, &item.path);
    }

    Ok(json!({
        "restored": true,
        "checkpoint_id": checkpoint_id,
        "changed_paths": changed_paths,
        "warnings": [],
    }))
}

fn canonical_project_root(root: &Path) -> Result<PathBuf, Value> {
    let root = root.canonicalize().map_err(|err| {
        fail(
            "invalid_project_root",
            format!("project root does not exist: {err}"),
        )
    })?;
    if !root.is_dir() {
        return Err(fail(
            "invalid_project_root",
            "project root must be a directory",
        ));
    }
    Ok(root)
}

fn branch_name(root: &Path) -> Result<Option<String>, Value> {
    let show_current = git_output(root, &["branch", "--show-current"], None, false)?;
    let name = String::from_utf8_lossy(&show_current.stdout)
        .trim()
        .to_string();
    if !name.is_empty() {
        return Ok(Some(name));
    }
    let abbrev = git_output(root, &["rev-parse", "--abbrev-ref", "HEAD"], None, false)?;
    let name = String::from_utf8_lossy(&abbrev.stdout).trim().to_string();
    if name.is_empty() || name == "HEAD" {
        Ok(None)
    } else {
        Ok(Some(name))
    }
}

fn git_text(root: &Path, args: &[&str], label: &str) -> Result<String, Value> {
    bounded_git_text(root, args, label, MAX_DIFF_BYTES)
}

fn bounded_git_text(
    root: &Path,
    args: &[&str],
    label: &str,
    max_bytes: usize,
) -> Result<String, Value> {
    let output = git_output(root, args, None, true)?;
    let data = output.stdout;
    if data.len() > max_bytes {
        return Err(fail_extra(
            "checkpoint_too_large",
            format!("{label} exceeds checkpoint v1 byte limit"),
            vec![
                ("byte_count", json!(data.len())),
                ("max_bytes", json!(max_bytes)),
            ],
        ));
    }
    let text = decode_utf8(&data, label)?;
    if text.starts_with("Binary files ") || text.contains("\nBinary files ") {
        return Err(fail(
            "unsupported_binary_diff",
            format!("{label} contains binary file changes; checkpoint v1 stores text diffs only"),
        ));
    }
    Ok(text)
}

fn git_output(
    root: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    check: bool,
) -> Result<Output, Value> {
    let mut command = Command::new("git");
    command.args(args).current_dir(root);
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|err| fail("git_exec_failed", err.to_string()))?;
    if let Some(input) = input {
        child
            .stdin
            .as_mut()
            .ok_or_else(|| fail("git_exec_failed", "git stdin unavailable"))?
            .write_all(input)
            .map_err(|err| fail("git_exec_failed", err.to_string()))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|err| fail("git_exec_failed", err.to_string()))?;
    if check && !output.status.success() {
        return Err(fail_extra(
            "git_failed",
            format!("git {} failed", args.join(" ")),
            vec![
                ("exit_code", json!(output.status.code().unwrap_or(-1))),
                ("stderr", json!(bounded_lossy(&output.stderr, 4000))),
            ],
        ));
    }
    Ok(output)
}

fn git_apply(root: &Path, args: &[&str], patch: &str) -> Result<(), String> {
    if patch.is_empty() {
        return Ok(());
    }
    let mut command = Command::new("git");
    command
        .arg("apply")
        .args(args)
        .arg("-")
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|err| format!("git apply failed to start: {err}"))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| "git apply stdin unavailable".to_string())?
        .write_all(patch.as_bytes())
        .map_err(|err| format!("failed to write git apply input: {err}"))?;
    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to wait for git apply: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git apply {} failed: {}",
            args.join(" "),
            bounded_lossy(&output.stderr, 4000)
        ));
    }
    Ok(())
}

fn decode_utf8(data: &[u8], label: &str) -> Result<String, Value> {
    if data.contains(&0) {
        return Err(fail(
            "binary_or_non_utf8_diff",
            format!("{label} contains NUL bytes"),
        ));
    }
    String::from_utf8(data.to_vec()).map_err(|_| {
        fail(
            "binary_or_non_utf8_diff",
            format!("{label} is not valid UTF-8"),
        )
    })
}

fn status_summary(status: &str) -> (Value, Vec<StatusEntry>) {
    let mut modified = 0usize;
    let mut added = 0usize;
    let mut deleted = 0usize;
    let mut renamed = 0usize;
    let mut copied = 0usize;
    let mut untracked = 0usize;
    let mut staged_count = 0usize;
    let mut unstaged_count = 0usize;
    let mut files = Vec::new();

    for line in status.lines() {
        if line.len() < 3 {
            continue;
        }
        let bytes = line.as_bytes();
        let x = bytes[0] as char;
        let y = bytes[1] as char;
        let mut path = line[3..].trim().trim_matches('"').to_string();
        if let Some((_, new_path)) = path.split_once(" -> ") {
            path = new_path.trim().trim_matches('"').to_string();
        }
        if path.is_empty() {
            continue;
        }
        let (label, staged, unstaged) = if x == '?' && y == '?' {
            untracked += 1;
            ("untracked", false, false)
        } else {
            let staged = x != ' ' && x != '?';
            let unstaged = y != ' ' && y != '?';
            if staged {
                staged_count += 1;
            }
            if unstaged {
                unstaged_count += 1;
            }
            let code = if x != ' ' && x != '?' { x } else { y };
            let label = match code {
                'A' => {
                    added += 1;
                    "added"
                }
                'D' => {
                    deleted += 1;
                    "deleted"
                }
                'R' => {
                    renamed += 1;
                    "renamed"
                }
                'C' => {
                    copied += 1;
                    "copied"
                }
                _ => {
                    modified += 1;
                    "modified"
                }
            };
            (label, staged, unstaged)
        };
        files.push(StatusEntry {
            path,
            status: label.to_string(),
            staged,
            unstaged,
        });
    }

    let files_json = files
        .iter()
        .map(|entry| {
            json!({
                "path": entry.path,
                "status": entry.status,
                "staged": entry.staged,
                "unstaged": entry.unstaged,
            })
        })
        .collect::<Vec<_>>();
    let counts = json!({
        "modified": modified,
        "added": added,
        "deleted": deleted,
        "renamed": renamed,
        "copied": copied,
        "untracked": untracked,
        "staged": staged_count,
        "unstaged": unstaged_count,
        "files": files.len(),
    });
    (
        json!({
            "counts": counts,
            "files": files_json,
            "clean": files.is_empty(),
        }),
        files,
    )
}

fn collect_untracked(
    root: &Path,
    include_untracked: bool,
) -> Result<(Vec<Value>, Vec<Value>), Value> {
    let raw = git_output(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        None,
        true,
    )?
    .stdout;
    let raw_paths = untracked_paths_from_status_z(&raw);
    if !include_untracked {
        let skipped = raw_paths
            .into_iter()
            .map(|path| skipped(path, "include_untracked_false", None))
            .collect();
        return Ok((Vec::new(), skipped));
    }

    let mut files = Vec::new();
    let mut skipped_files = Vec::new();
    let mut total = 0usize;
    for path in raw_paths {
        if files.len() >= MAX_UNTRACKED_FILES {
            skipped_files.push(skipped(path, "too_many_untracked_files", None));
            continue;
        }
        if invalid_rel_path(&path) || sensitive_path(&path) {
            skipped_files.push(skipped(path, "sensitive_or_invalid_path", None));
            continue;
        }
        let full = root.join(&path);
        let real = match full.canonicalize() {
            Ok(path) => path,
            Err(_) => {
                skipped_files.push(skipped(path, "not_found", None));
                continue;
            }
        };
        if !path_inside(root, &real) {
            skipped_files.push(skipped(path, "path_escapes_project", None));
            continue;
        }
        let metadata = match fs::symlink_metadata(&full) {
            Ok(metadata) => metadata,
            Err(_) => {
                skipped_files.push(skipped(path, "not_found", None));
                continue;
            }
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            skipped_files.push(skipped(path, "symlink", None));
            continue;
        }
        if !file_type.is_file() {
            skipped_files.push(skipped(path, "not_regular_file", None));
            continue;
        }
        let byte_count = metadata.len() as usize;
        if byte_count > MAX_UNTRACKED_BYTES {
            skipped_files.push(skipped(path, "too_large", Some(byte_count)));
            continue;
        }
        if total.saturating_add(byte_count) > MAX_UNTRACKED_TOTAL_BYTES {
            skipped_files.push(skipped(
                path,
                "total_untracked_budget_exceeded",
                Some(byte_count),
            ));
            continue;
        }
        let data = match fs::read(&full) {
            Ok(data) => data,
            Err(_) => {
                skipped_files.push(skipped(path, "read_error", None));
                continue;
            }
        };
        if data.len() > MAX_UNTRACKED_BYTES {
            skipped_files.push(skipped(path, "too_large", Some(data.len())));
            continue;
        }
        if binaryish(&data) {
            skipped_files.push(skipped(path, "binary_or_non_utf8", Some(data.len())));
            continue;
        }
        let content = match String::from_utf8(data.clone()) {
            Ok(content) => content,
            Err(_) => {
                skipped_files.push(skipped(path, "binary_or_non_utf8", Some(data.len())));
                continue;
            }
        };
        total += data.len();
        files.push(json!({
            "path": path,
            "content": content,
            "byte_count": data.len(),
            "sha256": sha256_hex_bytes(&data),
        }));
    }
    Ok((files, skipped_files))
}

fn untracked_paths_from_status_z(raw: &[u8]) -> Vec<String> {
    raw.split(|byte| *byte == 0)
        .filter_map(|entry| entry.strip_prefix(b"?? "))
        .map(|path| String::from_utf8_lossy(path).to_string())
        .collect()
}

fn validate_checkpoint_diff(label: &str, diff: &str) -> Result<(), Value> {
    if diff.len() > MAX_DIFF_BYTES {
        return Err(fail_extra(
            "invalid_checkpoint",
            format!("{label} exceeds checkpoint v1 byte limit"),
            vec![
                ("byte_count", json!(diff.len())),
                ("max_bytes", json!(MAX_DIFF_BYTES)),
            ],
        ));
    }
    for path in changed_paths_from_diff(diff) {
        if invalid_rel_path(&path) || sensitive_path(&path) {
            return Err(fail_extra(
                "invalid_checkpoint",
                "checkpoint diff contains an invalid or sensitive path",
                vec![("path", json!(path))],
            ));
        }
    }
    Ok(())
}

fn validate_untracked_checkpoint_files(
    root: &Path,
    values: &[Value],
) -> Result<Vec<UntrackedCheckpointFile>, Value> {
    let mut files = Vec::with_capacity(values.len());
    for item in values {
        let path = item.get("path").and_then(Value::as_str).ok_or_else(|| {
            fail(
                "invalid_checkpoint",
                "checkpoint contains invalid untracked file entry",
            )
        })?;
        let content = item.get("content").and_then(Value::as_str).ok_or_else(|| {
            fail(
                "invalid_checkpoint",
                "checkpoint contains invalid untracked file entry",
            )
        })?;
        if invalid_rel_path(path) || sensitive_path(path) {
            return Err(fail_extra(
                "invalid_checkpoint",
                "checkpoint contains invalid or sensitive untracked file entry",
                vec![("path", json!(path))],
            ));
        }
        let full = root.join(path);
        ensure_write_target_inside(root, &full).map_err(|err| {
            fail_extra(
                "invalid_checkpoint",
                "checkpoint untracked path escapes project",
                vec![("path", json!(path)), ("detail", json!(err))],
            )
        })?;
        if full.exists() {
            let metadata = fs::metadata(&full).map_err(|err| {
                fail_extra(
                    "invalid_checkpoint",
                    "failed to inspect checkpoint untracked path",
                    vec![("path", json!(path)), ("detail", json!(err.to_string()))],
                )
            })?;
            if !metadata.is_file() {
                return Err(fail_extra(
                    "invalid_checkpoint",
                    "checkpoint untracked path is not a regular file",
                    vec![("path", json!(path))],
                ));
            }
            let current = file_sha256(&full).map_err(|err| {
                fail_extra(
                    "invalid_checkpoint",
                    "failed to hash checkpoint untracked path",
                    vec![("path", json!(path)), ("detail", json!(err))],
                )
            })?;
            let expected = item
                .get("sha256")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| sha256_hex_bytes(content.as_bytes()));
            if current != expected {
                return Err(fail_extra(
                    "untracked_conflict",
                    "current file differs from checkpoint untracked content",
                    vec![("path", json!(path))],
                ));
            }
        }
        files.push(UntrackedCheckpointFile {
            path: path.to_string(),
            content: content.to_string(),
        });
    }
    Ok(files)
}

fn ensure_write_target_inside(root: &Path, full: &Path) -> Result<(), String> {
    if full.exists() {
        let real = full
            .canonicalize()
            .map_err(|err| format!("failed to canonicalize target: {err}"))?;
        if path_inside(root, &real) {
            return Ok(());
        }
        return Err("target escapes project".to_string());
    }

    let mut candidate = full.parent().unwrap_or(root).to_path_buf();
    while !candidate.exists() {
        let Some(parent) = candidate.parent() else {
            break;
        };
        candidate = parent.to_path_buf();
    }
    let real_parent = candidate
        .canonicalize()
        .map_err(|err| format!("failed to canonicalize parent: {err}"))?;
    if path_inside(root, &real_parent) {
        Ok(())
    } else {
        Err("parent escapes project".to_string())
    }
}

fn rollback_checkpoint(
    root: &Path,
    created_untracked: &[PathBuf],
    applied_checkpoint_staged_index: bool,
    applied_checkpoint_staged_worktree: bool,
    applied_checkpoint_unstaged: bool,
    staged_diff: &str,
    tracked_diff: &str,
) -> Result<(), String> {
    for path in created_untracked.iter().rev() {
        let _ = fs::remove_file(path);
    }
    if applied_checkpoint_unstaged {
        git_apply(root, &["--reverse"], tracked_diff)?;
    }
    if applied_checkpoint_staged_worktree {
        git_apply(root, &["--reverse"], staged_diff)?;
    }
    if applied_checkpoint_staged_index {
        git_apply(root, &["--reverse", "--cached"], staged_diff)?;
    }
    Ok(())
}

fn reapply_current(
    root: &Path,
    current_staged: &str,
    current_unstaged: &str,
) -> Result<(), String> {
    if !current_staged.is_empty() {
        git_apply(root, &["--cached"], current_staged)?;
        git_apply(root, &[], current_staged)?;
    }
    if !current_unstaged.is_empty() {
        git_apply(root, &[], current_unstaged)?;
    }
    Ok(())
}

fn invalid_rel_path(path: &str) -> bool {
    if path.is_empty() || path.contains('\0') || Path::new(path).is_absolute() {
        return true;
    }
    path.replace('\\', "/").split('/').any(|part| part == "..")
}

fn skipped(path: String, reason: &str, byte_count: Option<usize>) -> Value {
    let mut value = json!({
        "path": path,
        "reason": reason,
    });
    if let Some(byte_count) = byte_count {
        value["byte_count"] = json!(byte_count);
    }
    value
}

fn binaryish(data: &[u8]) -> bool {
    data.iter()
        .any(|byte| *byte == 0 || (*byte < 32 && !matches!(*byte, b'\t' | b'\n' | b'\r')))
}

fn changed_paths_from_diff(diff: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(pos) = rest.rfind(" b/") {
                push_unique(&mut paths, &rest[pos + 3..]);
            }
            continue;
        }
        for prefix in ["+++ b/", "--- a/"] {
            if let Some(path) = line.strip_prefix(prefix) {
                if path != "/dev/null" {
                    push_unique(&mut paths, path);
                }
            }
        }
    }
    paths
}

fn push_unique(paths: &mut Vec<String>, path: &str) {
    let path = path.trim();
    if path.is_empty() || paths.iter().any(|existing| existing == path) {
        return;
    }
    paths.push(path.to_string());
}

fn file_sha256(path: &Path) -> Result<String, String> {
    let data = fs::read(path).map_err(|err| err.to_string())?;
    Ok(sha256_hex_bytes(&data))
}

fn sha256_hex_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn path_inside(root: &Path, path: &Path) -> bool {
    path == root || path.starts_with(root)
}

fn bounded_lossy(data: &[u8], max_chars: usize) -> String {
    String::from_utf8_lossy(data)
        .chars()
        .take(max_chars)
        .collect()
}

fn fail(kind: &str, message: impl Into<String>) -> Value {
    fail_extra(kind, message, Vec::new())
}

fn fail_extra(kind: &str, message: impl Into<String>, extra: Vec<(&str, Value)>) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("error_kind".to_string(), json!(kind));
    obj.insert("error".to_string(), json!(message.into()));
    for (key, value) in extra {
        obj.insert(key.to_string(), value);
    }
    Value::Object(obj)
}
