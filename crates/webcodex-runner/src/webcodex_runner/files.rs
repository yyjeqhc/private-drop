use super::config::RunnerPolicy;
use super::output::CommandResult;
use super::shell::cwd_allowed;
use crate::project_overview::build_project_overview;
use crate::runner_config::DEFAULT_MAX_OUTPUT_BYTES;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Instant;
use webcodex_core::runner_operation::{RunnerFileOperation, RunnerFilePayload};
use webcodex_workspace::file_read_range::{self, ReadFileReason};

pub(crate) fn sha256_hex_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub(crate) fn resolve_requested_path(
    policy: &RunnerPolicy,
    cwd: Option<&str>,
    path: &str,
) -> Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("path cannot be empty".to_string());
    }
    let raw_path = PathBuf::from(path);
    let resolved = if raw_path.is_absolute() {
        raw_path
    } else {
        let base = cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
        base.join(raw_path)
    };
    let mut parent_for_policy = if resolved.exists() {
        resolved.clone()
    } else {
        resolved
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| resolved.clone())
    };
    while !parent_for_policy.exists() {
        let Some(parent) = parent_for_policy.parent() else {
            break;
        };
        parent_for_policy = parent.to_path_buf();
    }
    cwd_allowed(policy, &parent_for_policy)?;
    Ok(resolved)
}

#[cfg(test)]
pub(crate) fn is_basic_file_request_kind(kind: &str) -> bool {
    matches!(
        kind,
        "file_read"
            | "file_write"
            | "file_list"
            | "file_project_overview"
            | "file_delete_project_files"
            | "file_skill_list_packages"
            | "file_skill_read_file"
    )
}

pub(crate) fn handle_basic_file_request(
    policy: &RunnerPolicy,
    operation: &RunnerFileOperation,
    resolved: &Path,
    start: Instant,
) -> CommandResult {
    let request = operation.payload();
    match operation {
        RunnerFileOperation::Read(_) => handle_file_read_request(policy, request, resolved, start),
        RunnerFileOperation::Write(_) => {
            handle_file_write_request(policy, request, resolved, start)
        }
        RunnerFileOperation::List(_) => handle_file_list_request(resolved, start),
        RunnerFileOperation::SkillListPackages(_) => {
            handle_skill_list_packages_request(request, resolved, start)
        }
        RunnerFileOperation::SkillReadFile(_) => {
            handle_skill_read_file_request(policy, request, resolved, start)
        }
        RunnerFileOperation::ProjectOverview(_) => handle_project_overview_request(request, start),
        RunnerFileOperation::DeleteProjectFiles(_) => {
            handle_delete_project_files_request(request, resolved, start)
        }
        _ => CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some("file operation is not a basic file request".to_string()),
        },
    }
}

const MAX_DELETE_PROJECT_FILES_PATHS: usize = 64;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteProjectFilesPayload {
    paths: Vec<String>,
}

fn validate_delete_project_file_path(path: &str) -> bool {
    if path.is_empty() || path == "." || path.contains('\0') {
        return false;
    }
    let raw = Path::new(path);
    if raw.is_absolute() || crate::apply_edits_shared::is_sensitive_edit_path(path) {
        return false;
    }
    raw.components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn canonical_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut cursor = Some(path);
    while let Some(candidate) = cursor {
        match candidate.canonicalize() {
            Ok(canonical) => return Some(canonical),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                cursor = candidate.parent();
            }
            Err(_) => return None,
        }
    }
    None
}

fn delete_project_files_error(start: Instant, message: &'static str) -> CommandResult {
    CommandResult {
        exit_code: None,
        stdout: None,
        stderr: None,
        duration_ms: Some(start.elapsed().as_millis() as u64),
        error: Some(message.to_string()),
    }
}

fn handle_delete_project_files_request(
    request: &RunnerFilePayload,
    resolved_project_root: &Path,
    start: Instant,
) -> CommandResult {
    let Some(payload) = request
        .content
        .as_deref()
        .and_then(|raw| serde_json::from_str::<DeleteProjectFilesPayload>(raw).ok())
    else {
        return delete_project_files_error(start, "invalid delete_project_files payload");
    };
    if payload.paths.is_empty()
        || payload.paths.len() > MAX_DELETE_PROJECT_FILES_PATHS
        || payload
            .paths
            .iter()
            .any(|path| !validate_delete_project_file_path(path))
    {
        return delete_project_files_error(
            start,
            "delete_project_files request contains a refused path",
        );
    }
    let canonical_root = match resolved_project_root.canonicalize() {
        Ok(root) => root,
        Err(_) => {
            return delete_project_files_error(
                start,
                "delete_project_files project root is unavailable",
            )
        }
    };

    for path in &payload.paths {
        let target = canonical_root.join(path);
        match std::fs::symlink_metadata(&target) {
            Ok(metadata) => {
                if metadata.file_type().is_dir() {
                    return delete_project_files_error(
                        start,
                        "delete_project_files refuses directory targets",
                    );
                }
                let containment = if metadata.file_type().is_symlink() {
                    target
                        .parent()
                        .and_then(|parent| parent.canonicalize().ok())
                } else {
                    target.canonicalize().ok()
                };
                if !containment.as_ref().is_some_and(|candidate| {
                    webcodex_runner_config::paths::path_is_within(candidate, &canonical_root)
                }) {
                    return delete_project_files_error(
                        start,
                        "delete_project_files target is outside the project",
                    );
                }
                match std::fs::remove_file(&target) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => {
                        return delete_project_files_error(start, "delete_project_files failed")
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(ancestor) =
                    canonical_existing_ancestor(target.parent().unwrap_or(&canonical_root))
                else {
                    return delete_project_files_error(
                        start,
                        "delete_project_files parent is unavailable",
                    );
                };
                if !webcodex_runner_config::paths::path_is_within(&ancestor, &canonical_root) {
                    return delete_project_files_error(
                        start,
                        "delete_project_files target is outside the project",
                    );
                }
            }
            Err(_) => return delete_project_files_error(start, "delete_project_files failed"),
        }
    }

    let output = serde_json::json!({
        "deleted_paths": payload.paths,
    });
    CommandResult {
        exit_code: Some(0),
        stdout: Some(output.to_string()),
        stderr: Some(String::new()),
        duration_ms: Some(start.elapsed().as_millis() as u64),
        error: None,
    }
}

#[derive(Debug, Default, Deserialize)]
struct ProjectOverviewRunnerOptions {
    #[serde(default)]
    max_depth: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

fn handle_project_overview_request(request: &RunnerFilePayload, start: Instant) -> CommandResult {
    let Some(project_root) = request.cwd.as_deref() else {
        return CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some("project_overview request missing project root".to_string()),
        };
    };
    let requested_path = request.path.as_str();
    let options = match request.content.as_deref() {
        Some(payload) => match serde_json::from_str::<ProjectOverviewRunnerOptions>(payload) {
            Ok(options) => options,
            Err(error) => {
                return CommandResult {
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some(format!("invalid project_overview options: {error}")),
                }
            }
        },
        None => ProjectOverviewRunnerOptions::default(),
    };
    match build_project_overview(
        Path::new(project_root),
        requested_path,
        options.max_depth,
        options.limit,
    ) {
        Ok(output) => CommandResult {
            exit_code: Some(0),
            stdout: Some(output.to_string()),
            stderr: Some(String::new()),
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: None,
        },
        Err(error) => CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some(error),
        },
    }
}

fn read_file_reason_from_io(error: &std::io::Error) -> ReadFileReason {
    match error.kind() {
        std::io::ErrorKind::NotFound => ReadFileReason::NotFound,
        std::io::ErrorKind::PermissionDenied => ReadFileReason::PermissionDenied,
        std::io::ErrorKind::InvalidData => ReadFileReason::InvalidUtf8,
        _ => ReadFileReason::IoError,
    }
}

fn read_file_reason_message(reason: ReadFileReason) -> String {
    format!("read_file failed: {}", reason.as_str())
}

fn ensure_file_read_target_in_project(
    request: &RunnerFilePayload,
    resolved: &Path,
) -> Result<(), ReadFileReason> {
    let project_root = request.cwd.as_deref().ok_or(ReadFileReason::InvalidPath)?;
    let project_root = Path::new(project_root)
        .canonicalize()
        .map_err(|error| read_file_reason_from_io(&error))?;
    let target = resolved
        .canonicalize()
        .map_err(|error| read_file_reason_from_io(&error))?;
    if !target.starts_with(&project_root) {
        return Err(ReadFileReason::InvalidPath);
    }
    if !target.is_file() {
        return Err(ReadFileReason::NotFile);
    }
    Ok(())
}

fn handle_file_read_request(
    policy: &RunnerPolicy,
    request: &RunnerFilePayload,
    resolved: &Path,
    start: Instant,
) -> CommandResult {
    if let Err(reason) = ensure_file_read_target_in_project(request, resolved) {
        return CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some(read_file_reason_message(reason)),
        };
    }
    // The transport cap (`max_bytes`) is clamped to the shared content budget so
    // the runner never emits a range the server's model output limit cannot
    // hold. The server always sends an explicit `start_line`/`end_line` for
    // `read_file`, so this branch returns the canonical v1 envelope.
    let max = request
        .max_bytes
        .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES)
        .min(policy.max_output_bytes);
    if let (Some(start_line), Some(end_line)) = (request.start_line, request.end_line) {
        return handle_file_read_range_request(resolved, start_line, end_line, max, start);
    }

    // Legacy non-range path: read the whole file under the transport cap. This
    // is only reached by older callers that omit the line range; the server's
    // read_file tool always requests a range. No absolute path leaks: the error
    // carries a stable code, not `resolved.display()`.
    match std::fs::read(resolved) {
        Ok(bytes) => {
            if bytes.len() > max {
                CommandResult {
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some(format!(
                        "file too large: {} bytes exceeds max_bytes {}",
                        bytes.len(),
                        max
                    )),
                }
            } else {
                CommandResult {
                    exit_code: Some(0),
                    stdout: Some(String::from_utf8_lossy(&bytes).to_string()),
                    stderr: Some(String::new()),
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                    error: None,
                }
            }
        }
        Err(e) => CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some(file_read_error_message(&e)),
        },
    }
}

#[derive(Serialize)]
struct FileReadRangeEnvelope<'a> {
    format: &'static str,
    content: &'a str,
    sha256: &'a str,
    total_lines: usize,
    start_line: usize,
    limit: usize,
}

fn handle_file_read_range_request(
    resolved: &Path,
    start_line: usize,
    end_line: usize,
    max: usize,
    start: Instant,
) -> CommandResult {
    if start_line == 0 || end_line < start_line {
        return CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some("invalid line range for file_read".to_string()),
        };
    }

    // Reconstruct the shared effective range from the runner's inclusive
    // 1-based line window. The shared core owns line-counting, full-file
    // SHA-256, empty-line semantics, and content-budget enforcement, so the
    // runner no longer maintains its own range algorithm.
    let limit = end_line.saturating_sub(start_line).saturating_add(1);
    let range =
        webcodex_workspace::file_read_range::EffectiveRange::new(Some(start_line), Some(limit));
    match file_read_range::read_range_with_budget(resolved, range, max) {
        Ok(result) => {
            let envelope = FileReadRangeEnvelope {
                format: "webcodex.file_read_range.v1",
                content: &result.content,
                sha256: &result.sha256,
                total_lines: result.total_lines,
                start_line: result.start_line,
                limit: result.limit,
            };
            let serialized = match serde_json::to_vec(&envelope) {
                Ok(serialized) => serialized,
                Err(_) => {
                    return CommandResult {
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        duration_ms: Some(start.elapsed().as_millis() as u64),
                        error: Some(read_file_reason_message(ReadFileReason::IoError)),
                    }
                }
            };
            let output_cap = max.min(file_read_range::MAX_SERIALIZED_OUTPUT_BYTES);
            if serialized.len() > output_cap {
                return CommandResult {
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some("range output too large".to_string()),
                };
            }
            CommandResult {
                exit_code: Some(0),
                stdout: Some(String::from_utf8(serialized).expect("JSON serialization is UTF-8")),
                stderr: Some(String::new()),
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: None,
            }
        }
        Err(error) => CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some(read_file_reason_message(error.reason)),
        },
    }
}

#[derive(Debug, Deserialize)]
struct SkillPackageListOptions {
    limit: usize,
}

#[derive(Debug, Serialize)]
struct SkillPackageListEntry {
    name: String,
    kind: &'static str,
}

#[derive(Debug, Serialize)]
struct SkillPackageListEnvelope {
    format: &'static str,
    entries: Vec<SkillPackageListEntry>,
    truncated: bool,
}

#[derive(Debug, Deserialize)]
struct SkillReadOptions {
    package_root: String,
    max_file_bytes: usize,
}

#[derive(Debug, Serialize)]
struct SkillFileReadEnvelope<'a> {
    format: &'static str,
    content: &'a str,
    sha256: &'a str,
    file_bytes: usize,
    total_lines: usize,
    start_line: usize,
    limit: usize,
    returned_lines: usize,
    end_line: Option<usize>,
    has_more: bool,
    next_start_line: Option<usize>,
}

fn skill_command_error(start: Instant, code: &'static str) -> CommandResult {
    CommandResult {
        exit_code: None,
        stdout: None,
        stderr: None,
        duration_ms: Some(start.elapsed().as_millis() as u64),
        error: Some(code.to_string()),
    }
}

fn skill_list_success(
    start: Instant,
    entries: Vec<SkillPackageListEntry>,
    truncated: bool,
) -> CommandResult {
    let output = SkillPackageListEnvelope {
        format: "webcodex.skill_package_list.v1",
        entries,
        truncated,
    };
    match serde_json::to_string(&output) {
        Ok(stdout) => CommandResult {
            exit_code: Some(0),
            stdout: Some(stdout),
            stderr: Some(String::new()),
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: None,
        },
        Err(_) => skill_command_error(start, "skill_list_unavailable"),
    }
}

fn canonical_skill_package_root(path: &str) -> bool {
    if path.contains(['\\', '\0']) {
        return false;
    }
    let components = path.split('/').collect::<Vec<_>>();
    components.len() == 3
        && components[0] == ".agents"
        && components[1] == "skills"
        && !components[2].is_empty()
        && components[2] != "."
        && components[2] != ".."
}

fn canonical_skill_resource_request_path(package_root: &str, path: &str) -> bool {
    let Some(resource) = path
        .strip_prefix(package_root)
        .and_then(|rest| rest.strip_prefix('/'))
    else {
        return false;
    };
    !resource.is_empty()
        && !resource.contains(['\\', '\0'])
        && resource
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

fn handle_skill_list_packages_request(
    request: &RunnerFilePayload,
    resolved: &Path,
    start: Instant,
) -> CommandResult {
    if request.path != ".agents/skills" {
        return skill_command_error(start, "skill_path_invalid");
    }
    let options = match request
        .content
        .as_deref()
        .and_then(|value| serde_json::from_str::<SkillPackageListOptions>(value).ok())
    {
        Some(options) if (1..=257).contains(&options.limit) => options,
        _ => return skill_command_error(start, "skill_list_invalid_options"),
    };
    let project_root = match request
        .cwd
        .as_deref()
        .and_then(|path| Path::new(path).canonicalize().ok())
    {
        Some(root) => root,
        None => return skill_command_error(start, "skill_list_unavailable"),
    };
    let metadata = match std::fs::symlink_metadata(resolved) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return skill_list_success(start, Vec::new(), false)
        }
        Err(_) => return skill_command_error(start, "skill_list_unavailable"),
    };
    if metadata.file_type().is_symlink() {
        return skill_command_error(start, "skill_path_escape");
    }
    let canonical = match resolved.canonicalize() {
        Ok(path) => path,
        Err(_) => return skill_command_error(start, "skill_list_unavailable"),
    };
    if !canonical.starts_with(&project_root) || !canonical.is_dir() {
        return skill_command_error(start, "skill_path_escape");
    }
    let directory = match std::fs::read_dir(&canonical) {
        Ok(directory) => directory,
        Err(_) => return skill_command_error(start, "skill_list_unavailable"),
    };
    let mut retained = std::collections::BTreeSet::<(String, &'static str)>::new();
    let mut candidate_count = 0usize;
    for entry in directory {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => return skill_command_error(start, "skill_list_unavailable"),
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => return skill_command_error(start, "skill_list_unavailable"),
        };
        let kind = if file_type.is_dir() {
            "dir"
        } else if file_type.is_symlink() {
            "symlink"
        } else {
            continue;
        };
        candidate_count = candidate_count.saturating_add(1);
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        retained.insert((name, kind));
        if retained.len() > options.limit {
            if let Some(last) = retained.iter().next_back().cloned() {
                retained.remove(&last);
            }
        }
    }
    let entries = retained
        .into_iter()
        .map(|(name, kind)| SkillPackageListEntry { name, kind })
        .collect();
    skill_list_success(start, entries, candidate_count > options.limit)
}

fn handle_skill_read_file_request(
    policy: &RunnerPolicy,
    request: &RunnerFilePayload,
    resolved: &Path,
    start: Instant,
) -> CommandResult {
    const MAX_SKILL_FILE_BYTES: usize = 512 * 1024;
    const MAX_SKILL_INTERNAL_OUTPUT_BYTES: usize = 512 * 1024;
    let options = match request
        .content
        .as_deref()
        .and_then(|value| serde_json::from_str::<SkillReadOptions>(value).ok())
    {
        Some(options)
            if !options.package_root.trim().is_empty()
                && (1..=MAX_SKILL_FILE_BYTES).contains(&options.max_file_bytes) =>
        {
            options
        }
        _ => return skill_command_error(start, "skill_path_invalid"),
    };
    if !canonical_skill_package_root(&options.package_root)
        || !canonical_skill_resource_request_path(&options.package_root, &request.path)
    {
        return skill_command_error(start, "skill_path_invalid");
    }
    let Some(project_root_raw) = request.cwd.as_deref() else {
        return skill_command_error(start, "skill_path_invalid");
    };
    let project_root = match Path::new(project_root_raw).canonicalize() {
        Ok(root) => root,
        Err(_) => return skill_command_error(start, "skill_path_invalid"),
    };
    let package_raw = project_root.join(&options.package_root);
    let package_meta = match std::fs::symlink_metadata(&package_raw) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return skill_command_error(start, "skill_file_not_found")
        }
        Err(_) => return skill_command_error(start, "skill_path_invalid"),
    };
    if package_meta.file_type().is_symlink() || !package_meta.is_dir() {
        return skill_command_error(start, "skill_path_escape");
    }
    let package_root = match package_raw.canonicalize() {
        Ok(root) => root,
        Err(_) => return skill_command_error(start, "skill_path_invalid"),
    };
    if !package_root.starts_with(&project_root) {
        return skill_command_error(start, "skill_path_escape");
    }
    let target = match resolved.canonicalize() {
        Ok(target) => target,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return skill_command_error(start, "skill_file_not_found")
        }
        Err(_) => return skill_command_error(start, "skill_path_invalid"),
    };
    if !target.starts_with(&package_root) || !target.starts_with(&project_root) || !target.is_file()
    {
        return skill_command_error(start, "skill_path_escape");
    }
    let requested_project_relative = request.path.as_str();
    let canonical_project_relative = match target.strip_prefix(&project_root) {
        Ok(relative) => relative.to_string_lossy(),
        Err(_) => return skill_command_error(start, "skill_path_escape"),
    };
    if webcodex_core::sensitive_paths::is_secret_path(requested_project_relative)
        || webcodex_core::sensitive_paths::is_secret_path(canonical_project_relative.as_ref())
    {
        return skill_command_error(start, "skill_sensitive_path");
    }
    let file_bytes = match target.metadata() {
        Ok(metadata) => metadata.len().min(usize::MAX as u64) as usize,
        Err(_) => return skill_command_error(start, "skill_path_invalid"),
    };
    if file_bytes > options.max_file_bytes {
        return skill_command_error(start, "skill_file_too_large");
    }
    let (Some(start_line), Some(end_line)) = (request.start_line, request.end_line) else {
        return skill_command_error(start, "skill_path_invalid");
    };
    if start_line == 0 || end_line < start_line {
        return skill_command_error(start, "skill_path_invalid");
    }
    let limit = end_line.saturating_sub(start_line).saturating_add(1);
    let range = file_read_range::EffectiveRange::new(Some(start_line), Some(limit));
    let text_budget = request
        .max_bytes
        .unwrap_or(48 * 1024)
        .min(policy.max_output_bytes)
        .min(file_read_range::MAX_RANGE_CONTENT_BYTES);
    let result = match file_read_range::read_range_with_budget(&target, range, text_budget) {
        Ok(result) => result,
        Err(error) => {
            return skill_command_error(
                start,
                match error.reason {
                    ReadFileReason::InvalidUtf8 => "skill_invalid_utf8",
                    ReadFileReason::RangeTooLarge => "skill_read_output_too_large",
                    ReadFileReason::NotFound => "skill_file_not_found",
                    _ => "skill_read_unavailable",
                },
            )
        }
    };
    let envelope = SkillFileReadEnvelope {
        format: "webcodex.skill_file_read.v1",
        content: &result.content,
        sha256: &result.sha256,
        file_bytes,
        total_lines: result.total_lines,
        start_line: result.start_line,
        limit: result.limit,
        returned_lines: result.returned_lines,
        end_line: result.end_line,
        has_more: result.has_more,
        next_start_line: result.next_start_line,
    };
    let serialized = match serde_json::to_vec(&envelope) {
        Ok(serialized) => serialized,
        Err(_) => return skill_command_error(start, "skill_read_unavailable"),
    };
    let output_cap = policy.max_output_bytes.min(MAX_SKILL_INTERNAL_OUTPUT_BYTES);
    if serialized.len() > output_cap {
        return skill_command_error(start, "skill_read_output_too_large");
    }
    CommandResult {
        exit_code: Some(0),
        stdout: Some(String::from_utf8(serialized).expect("JSON serialization is UTF-8")),
        stderr: Some(String::new()),
        duration_ms: Some(start.elapsed().as_millis() as u64),
        error: None,
    }
}

/// Map a raw IO error from the legacy whole-file read path to a stable
/// path-free message.
fn file_read_error_message(error: &std::io::Error) -> String {
    read_file_reason_message(read_file_reason_from_io(error))
}

fn handle_file_write_request(
    policy: &RunnerPolicy,
    request: &RunnerFilePayload,
    resolved: &Path,
    start: Instant,
) -> CommandResult {
    let content = request.content.clone().unwrap_or_default();
    if content.len() > policy.max_output_bytes {
        return CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some(format!(
                "content too large: {} bytes exceeds max_output_bytes {}",
                content.len(),
                policy.max_output_bytes
            )),
        };
    }
    if let Some(expected) = request.expected_sha256.as_deref() {
        match std::fs::read(resolved) {
            Ok(existing) => {
                let actual = sha256_hex_bytes(&existing);
                if !actual.eq_ignore_ascii_case(expected) {
                    return CommandResult {
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        duration_ms: Some(start.elapsed().as_millis() as u64),
                        error: Some(format!(
                            "expected_sha256 mismatch: expected {}, actual {}",
                            expected, actual
                        )),
                    };
                }
            }
            Err(e) => {
                return CommandResult {
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some(format!(
                        "failed to read existing file for expected_sha256 {}: {}",
                        resolved.display(),
                        e
                    )),
                };
            }
        }
    }
    if request.create_dirs {
        if let Some(parent) = resolved.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return CommandResult {
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some(format!(
                        "failed to create parent directory {}: {}",
                        parent.display(),
                        e
                    )),
                };
            }
        }
    }
    match std::fs::write(resolved, content.as_bytes()) {
        Ok(()) => CommandResult {
            exit_code: Some(0),
            stdout: Some(content.len().to_string()),
            stderr: Some(String::new()),
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: None,
        },
        Err(e) => CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some(format!("failed to write {}: {}", resolved.display(), e)),
        },
    }
}

fn handle_file_list_request(resolved: &Path, start: Instant) -> CommandResult {
    match std::fs::read_dir(resolved) {
        Ok(entries) => {
            let mut names = Vec::new();
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let suffix = entry
                    .file_type()
                    .ok()
                    .filter(|t| t.is_dir())
                    .map(|_| "/")
                    .unwrap_or("");
                names.push(format!("{}{}", name, suffix));
            }
            names.sort();
            CommandResult {
                exit_code: Some(0),
                stdout: Some(format!("{}\n", names.join("\n"))),
                stderr: Some(String::new()),
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: None,
            }
        }
        Err(e) => CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some(format!("failed to list {}: {}", resolved.display(), e)),
        },
    }
}
