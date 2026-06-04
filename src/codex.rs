use crate::projects::{ProjectConfig, ProjectsConfig, SshConfig};
use salvo::prelude::*;
mod artifact;
mod capabilities;
mod command_request;
mod command_workflow;
mod context;
mod edit;
mod git;
mod jobs;
mod patch;
mod remote_edit;
mod report;
mod security;
mod shell;
mod source;
mod ssh;
mod trusted;
mod types;
mod url_security;
pub use artifact::codex_artifact;
pub use capabilities::codex_projects;
#[cfg(test)]
use command_request::*;
pub use command_request::{
    codex_check, codex_command, codex_command_approve, codex_command_reject, codex_command_request,
    codex_command_request_batch, codex_command_request_op, codex_command_request_raw,
    codex_command_requests,
};
use context::*;
pub use context::{codex_context, codex_context_batch};
pub use edit::codex_edit;
use edit::*;
pub use git::codex_git;
#[cfg(test)]
use git::*;
pub use jobs::codex_job;
pub use patch::codex_apply_patch;
use remote_edit::*;
pub use report::codex_report;
pub use security::is_sensitive_path;
use shell::*;
use ssh::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use types::*;
#[cfg(test)]
use url_security::*;
// =============================================================================
// Request / Response types
// =============================================================================

// =============================================================================
// Constants
// =============================================================================

pub(super) const MAX_OUTPUT_LEN: usize = 50_000;
pub(super) const CHECK_TIMEOUT_SECS: u64 = 300;

// =============================================================================
// Helpers
// =============================================================================

pub(super) fn get_projects(depot: &Depot) -> Option<Arc<ProjectsConfig>> {
    depot.obtain::<Arc<ProjectsConfig>>().ok().cloned()
}

pub(super) fn truncate_string(s: String, max_len: usize) -> (String, bool) {
    if s.len() <= max_len {
        (s, false)
    } else {
        let mut end = max_len;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        (s[..end].to_string(), true)
    }
}

// =============================================================================
// SSH helpers
// =============================================================================

/// Run a command in the project directory.
/// For SSH: wraps with `cd <path> && <cmd>`.
/// For local: delegates to run_command with cwd.
pub(super) fn run_project_cmd(
    proj: &ProjectConfig,
    cmd: &str,
    timeout_secs: u64,
    ssh_config: Option<&SshConfig>,
) -> (i32, String, String, u64) {
    if proj.is_ssh() {
        let ssh_targets = match build_ssh_targets(proj) {
            Ok(t) => t,
            Err(e) => return (-1, String::new(), e, 0),
        };
        let remote_cmd = format!("cd {} && {}", shell_escape(&proj.path), cmd);
        run_ssh_targets(&ssh_targets, &remote_cmd, timeout_secs, ssh_config)
    } else {
        run_command(cmd, &proj.root(), timeout_secs)
    }
}

/// Validate a path for SSH read_file operations.
fn validate_ssh_read_path(rel_path: &str) -> Result<(), String> {
    if rel_path.starts_with('/') {
        return Err("Absolute paths are not allowed".to_string());
    }
    if rel_path.contains("..") {
        return Err("Path traversal (..) is not allowed".to_string());
    }
    if is_sensitive_path(rel_path) {
        return Err(format!("Cannot access sensitive path: {}", rel_path));
    }
    Ok(())
}

// =============================================================================
// SSH context helpers
// =============================================================================

pub(super) fn ssh_overview(
    proj: &ProjectConfig,
    project_name: &str,
    ssh_config: Option<&SshConfig>,
) -> ContextResponse {
    let ssh_targets = match build_ssh_targets(proj) {
        Ok(t) => t,
        Err(e) => {
            return ContextResponse {
                success: false,
                project: project_name.to_string(),
                mode: "overview".to_string(),
                content: None,
                items: None,
                truncated: false,
                error: Some(e),
            }
        }
    };
    let important_files = [
        "README.md",
        "TODO.md",
        "Cargo.toml",
        "scripts/e2e_test.sh",
        "src/main.rs",
    ];
    let file_args = important_files
        .iter()
        .map(|f| shell_escape(f))
        .collect::<Vec<_>>()
        .join(" ");
    let remote_cmd = format!(
        "cd {} || exit 2; printf '__BRANCH__\\n'; git rev-parse --abbrev-ref HEAD 2>/dev/null || printf 'unknown\\n'; printf '__STATUS__\\n'; git status --short --untracked-files=no 2>/dev/null || true; printf '__FILES__\\n'; for f in {}; do if test -f \"$f\"; then printf '%s=yes\\n' \"$f\"; else printf '%s=no\\n' \"$f\"; fi; done",
        shell_escape(&proj.path),
        file_args
    );
    let (code, stdout, stderr, _) = run_ssh_targets(&ssh_targets, &remote_cmd, 15, ssh_config);
    if code != 0 {
        return ContextResponse {
            success: false,
            project: project_name.to_string(),
            mode: "overview".to_string(),
            content: None,
            items: None,
            truncated: false,
            error: Some(format!("SSH overview failed: {}", stderr.trim())),
        };
    }

    let mut section = "";
    let mut branch = "unknown".to_string();
    let mut status_lines: Vec<String> = Vec::new();
    let mut file_status: HashMap<String, String> = HashMap::new();
    for line in stdout.lines() {
        match line {
            "__BRANCH__" => section = "branch",
            "__STATUS__" => section = "status",
            "__FILES__" => section = "files",
            _ => match section {
                "branch" if !line.trim().is_empty() => branch = line.trim().to_string(),
                "status" => status_lines.push(line.to_string()),
                "files" => {
                    if let Some((path, exists)) = line.split_once('=') {
                        file_status.insert(path.to_string(), exists.to_string());
                    }
                }
                _ => {}
            },
        }
    }
    let status = status_lines.join("\n");
    let mut content = format!(
        "Project: {}\nRoot: {}\nBranch: {}\n\nGit Status:\n{}\n\nAllowed Checks: {}\n\nImportant Files:",
        project_name,
        proj.path,
        branch,
        status.trim(),
        proj.allowed_checks.join(", ")
    );
    for f in &important_files {
        let exists = file_status.get(*f).map(String::as_str).unwrap_or("no");
        content.push_str(&format!(
            "\n  {}: {}",
            f,
            if exists == "yes" { "yes" } else { "no" }
        ));
    }
    ContextResponse {
        success: true,
        project: project_name.to_string(),
        mode: "overview".to_string(),
        content: Some(content),
        items: None,
        truncated: false,
        error: None,
    }
}

pub(super) fn ssh_tree(
    proj: &ProjectConfig,
    project_name: &str,
    rel_path: Option<&str>,
    limit: usize,
    max_depth: usize,
    ssh_config: Option<&SshConfig>,
) -> ContextResponse {
    let mut excludes = String::new();
    for dir in IGNORED_DIRS {
        excludes.push_str(&format!(" -not -path '*/{}/*'", dir));
    }
    let limit = normalize_tree_limit(limit);
    let max_depth = normalize_tree_depth(max_depth);
    let find_root = match rel_path {
        Some(path) => {
            if let Err(e) = validate_ssh_read_path(path) {
                return context_error(project_name, &ContextMode::Tree, e);
            }
            shell_escape(path)
        }
        None => shell_escape("."),
    };
    let cmd = format!(
        "cd {} && find {} -mindepth 1 -maxdepth {}{} -type f -print 2>/dev/null | sort | head -n {} | sed 's|^\\./||'",
        shell_escape(&proj.path), find_root, max_depth, excludes, limit
    );
    let ssh_targets = match build_ssh_targets(proj) {
        Ok(t) => t,
        Err(e) => return context_error(project_name, &ContextMode::Tree, e),
    };
    let (code, stdout, stderr, _) = run_ssh_targets(&ssh_targets, &cmd, 30, ssh_config);
    if code != 0 {
        return ContextResponse {
            success: false,
            project: project_name.to_string(),
            mode: "tree".to_string(),
            content: None,
            items: None,
            truncated: false,
            error: Some(format!("SSH tree failed: {}", stderr.trim())),
        };
    }
    let mut items: Vec<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();
    let truncated = items.len() >= limit;
    items.truncate(limit);
    ContextResponse {
        success: true,
        project: project_name.to_string(),
        mode: "tree".to_string(),
        content: None,
        items: Some(items),
        truncated,
        error: None,
    }
}

pub(super) fn ssh_search(
    proj: &ProjectConfig,
    project_name: &str,
    query: &str,
    ssh_config: Option<&SshConfig>,
) -> ContextResponse {
    // Build grep exclusions
    let mut excludes = String::new();
    for dir in IGNORED_DIRS {
        excludes.push_str(&format!(" --exclude-dir='{}'", dir));
    }
    // Use grep -rn, then head to limit results
    let escaped_query = query.replace('\'', "'\\''");
    let cmd = format!(
        "cd {} && grep -rn{} --include='*' '{}' . 2>/dev/null | head -n {} | sed 's|^\\./||'",
        shell_escape(&proj.path),
        excludes,
        escaped_query,
        MAX_SEARCH_RESULTS
    );
    let ssh_targets = match build_ssh_targets(proj) {
        Ok(t) => t,
        Err(e) => return context_error(project_name, &ContextMode::Search, e),
    };
    let (code, stdout, stderr, _) = run_ssh_targets(&ssh_targets, &cmd, 30, ssh_config);
    // grep returns 1 if no match, that's ok
    if code != 0 && code != 1 {
        return ContextResponse {
            success: false,
            project: project_name.to_string(),
            mode: "search".to_string(),
            content: None,
            items: None,
            truncated: false,
            error: Some(format!("SSH search failed: {}", stderr.trim())),
        };
    }
    let items: Vec<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();
    let truncated = items.len() >= MAX_SEARCH_RESULTS;
    ContextResponse {
        success: true,
        project: project_name.to_string(),
        mode: "search".to_string(),
        content: None,
        items: Some(items),
        truncated,
        error: None,
    }
}

pub(super) fn ssh_grep_context(
    proj: &ProjectConfig,
    project_name: &str,
    rel_path: Option<&str>,
    query: &str,
    limit: usize,
    ssh_config: Option<&SshConfig>,
) -> ContextResponse {
    if let Some(path) = rel_path {
        if let Err(e) = validate_ssh_read_path(path) {
            return context_error(project_name, &ContextMode::GrepContext, e);
        }
    }
    let mut excludes = String::new();
    for dir in IGNORED_DIRS {
        excludes.push_str(&format!(" --exclude-dir='{}'", dir));
    }
    let search_root = shell_escape(rel_path.unwrap_or("."));
    let escaped_query = query.replace('\'', "'\\''");
    let limit = limit.clamp(1, MAX_READ_FILE_LIMIT);
    let cmd = format!(
        "cd {} && grep -R -n -C 3{} --include='*' '{}' {} 2>/dev/null | head -n {} | sed 's|^\\./||' | awk '{{ if(length($0)>{}) print substr($0,1,{}) \"… [line truncated]\"; else print }}'",
        shell_escape(&proj.path),
        excludes,
        escaped_query,
        search_root,
        limit,
        MAX_CONTEXT_LINE_LEN,
        MAX_CONTEXT_LINE_LEN
    );
    let ssh_targets = match build_ssh_targets(proj) {
        Ok(t) => t,
        Err(e) => return context_error(project_name, &ContextMode::GrepContext, e),
    };
    let (code, stdout, stderr, _) = run_ssh_targets(&ssh_targets, &cmd, 30, ssh_config);
    if code != 0 && code != 1 {
        return context_error(
            project_name,
            &ContextMode::GrepContext,
            format!("SSH grep_context failed: {}", stderr.trim()),
        );
    }
    mode_content_response(project_name, "grep_context", stdout, MAX_OUTPUT_LEN)
}

pub(super) fn ssh_read_file(
    proj: &ProjectConfig,
    project_name: &str,
    rel_path: &str,
    start_line: usize,
    limit: usize,
    ssh_config: Option<&SshConfig>,
) -> ContextResponse {
    if let Err(e) = validate_ssh_read_path(rel_path) {
        return ContextResponse {
            success: false,
            project: project_name.to_string(),
            mode: "read_file".to_string(),
            content: None,
            items: None,
            truncated: false,
            error: Some(e),
        };
    }
    let end_line = match validate_read_file_range(start_line, limit) {
        Ok(end_line) => end_line,
        Err(e) => {
            return ContextResponse {
                success: false,
                project: project_name.to_string(),
                mode: "read_file".to_string(),
                content: None,
                items: None,
                truncated: false,
                error: Some(e),
            }
        }
    };
    let escaped_path = shell_escape(rel_path);
    let cmd = format!(
        "sed -n '{},{}p' -- {} | awk '{{ if(length($0)>{}) print substr($0,1,{}) \"… [line truncated]\"; else print }}'",
        start_line, end_line, escaped_path, MAX_CONTEXT_LINE_LEN, MAX_CONTEXT_LINE_LEN
    );
    let (code, stdout, stderr, _) = run_project_cmd(proj, &cmd, 30, ssh_config);
    if code != 0 {
        return ContextResponse {
            success: false,
            project: project_name.to_string(),
            mode: "read_file".to_string(),
            content: None,
            items: None,
            truncated: false,
            error: Some(format!("Failed to read file: {}", stderr.trim())),
        };
    }
    // Add line numbers like the local version
    let lines: Vec<String> = stdout
        .lines()
        .enumerate()
        .map(|(i, l)| format_context_line(start_line + i, l).0)
        .collect();
    let output = lines.join("\n");
    let (output, truncated) = truncate_string(output, MAX_OUTPUT_LEN);
    ContextResponse {
        success: true,
        project: project_name.to_string(),
        mode: "read_file".to_string(),
        content: Some(output),
        items: None,
        truncated,
        error: None,
    }
}

pub(super) fn agent_context_shell_fragment() -> String {
    let files = AGENT_CONTEXT_FILES
        .iter()
        .map(|f| shell_escape(f))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        " printf '# Agent context\\n\\nLoaded project rules and memory files for alignment before planning or editing.\\n'; for f in {}; do printf '\\n## %s\\n\\n' \"$f\"; if test -f \"$f\"; then sed -n '1,240p' -- \"$f\"; else printf '(missing)\\n'; fi; done;",
        files
    )
}

fn ssh_overview_from_batch_block(
    proj: &ProjectConfig,
    project_name: &str,
    block: &str,
) -> ContextResponse {
    let important_files = [
        "README.md",
        "TODO.md",
        "Cargo.toml",
        "scripts/e2e_test.sh",
        "src/main.rs",
    ];
    let mut section = "";
    let mut branch = "unknown".to_string();
    let mut status_lines: Vec<String> = Vec::new();
    let mut file_status: HashMap<String, String> = HashMap::new();
    for line in block.lines() {
        match line {
            "__BRANCH__" => section = "branch",
            "__STATUS__" => section = "status",
            "__FILES__" => section = "files",
            _ => match section {
                "branch" if !line.trim().is_empty() => branch = line.trim().to_string(),
                "status" => status_lines.push(line.to_string()),
                "files" => {
                    if let Some((path, exists)) = line.split_once('=') {
                        file_status.insert(path.to_string(), exists.to_string());
                    }
                }
                _ => {}
            },
        }
    }
    let status = status_lines.join("\n");
    let mut content = format!(
        "Project: {}\nRoot: {}\nBranch: {}\n\nGit Status:\n{}\n\nAllowed Checks: {}\n\nImportant Files:",
        project_name,
        proj.path,
        branch,
        status.trim(),
        proj.allowed_checks.join(", ")
    );
    for f in &important_files {
        let exists = file_status.get(*f).map(String::as_str).unwrap_or("no");
        content.push_str(&format!(
            "\n  {}: {}",
            f,
            if exists == "yes" { "yes" } else { "no" }
        ));
    }
    ContextResponse {
        success: true,
        project: project_name.to_string(),
        mode: "overview".to_string(),
        content: Some(content),
        items: None,
        truncated: false,
        error: None,
    }
}

fn ssh_batch_block_to_response(
    proj: &ProjectConfig,
    project_name: &str,
    item: &ContextBatchItem,
    block: &str,
) -> ContextResponse {
    if let Some(err) = block.strip_prefix("__PDCTX_ERROR__:") {
        return context_error(project_name, &item.mode, err.trim().to_string());
    }
    match item.mode {
        ContextMode::Overview => ssh_overview_from_batch_block(proj, project_name, block),
        ContextMode::Tree => {
            let mut items: Vec<String> = block
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect();
            let truncated = items.len() >= MAX_TREE_ITEMS;
            items.truncate(MAX_TREE_ITEMS);
            ContextResponse {
                success: true,
                project: project_name.to_string(),
                mode: "tree".to_string(),
                content: None,
                items: Some(items),
                truncated,
                error: None,
            }
        }
        ContextMode::ReadFile => {
            let lines: Vec<String> = block
                .lines()
                .enumerate()
                .map(|(i, l)| format!("{:4} | {}", item.start_line + i, l))
                .collect();
            let (output, truncated) = truncate_string(lines.join("\n"), MAX_OUTPUT_LEN);
            ContextResponse {
                success: true,
                project: project_name.to_string(),
                mode: "read_file".to_string(),
                content: Some(output),
                items: None,
                truncated,
                error: None,
            }
        }
        ContextMode::MarkdownOutline => mode_content_response(
            project_name,
            "markdown_outline",
            block.to_string(),
            MAX_OUTPUT_LEN,
        ),
        ContextMode::ReadSection => mode_content_response(
            project_name,
            "read_section",
            block.to_string(),
            MAX_OUTPUT_LEN,
        ),
        ContextMode::AgentContext => mode_content_response(
            project_name,
            "agent_context",
            block.to_string(),
            MAX_OUTPUT_LEN,
        ),
        ContextMode::GitStatus => {
            let (content, truncated) = truncate_string(block.to_string(), MAX_OUTPUT_LEN);
            ContextResponse {
                success: true,
                project: project_name.to_string(),
                mode: "git_status".to_string(),
                content: Some(content),
                items: None,
                truncated,
                error: None,
            }
        }
        ContextMode::GitDiff => {
            let (content, truncated) = truncate_string(block.to_string(), MAX_OUTPUT_LEN);
            ContextResponse {
                success: true,
                project: project_name.to_string(),
                mode: "git_diff".to_string(),
                content: Some(content),
                items: None,
                truncated,
                error: None,
            }
        }
        ContextMode::Search | ContextMode::GrepContext | ContextMode::ExperimentOutputs => {
            // ExperimentOutputs is not supported in single-SSH batch mode; run it standalone.
            context_error(
                project_name,
                &item.mode,
                "search-like and experiment_outputs modes are not supported by single-SSH context batch".to_string(),
            )
        }
    }
}

pub(super) fn ssh_context_batch_error_results(
    project_name: &str,
    requests: &[ContextBatchItem],
    error: String,
) -> Vec<ContextResponse> {
    requests
        .iter()
        .map(|item| context_error(project_name, &item.mode, error.clone()))
        .collect()
}

pub(super) fn try_ssh_context_batch_once(
    proj: &ProjectConfig,
    project_name: &str,
    requests: &[ContextBatchItem],
    ssh_config: Option<&SshConfig>,
) -> Option<(Vec<ContextResponse>, u64)> {
    if requests.is_empty() {
        return Some((Vec::new(), 0));
    }
    let ssh_targets = match build_ssh_targets(proj) {
        Ok(t) => t,
        Err(e) => {
            return Some((
                ssh_context_batch_error_results(project_name, requests, e),
                0,
            ))
        }
    };

    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let mut script = format!("cd {} || exit 2;", shell_escape(&proj.path));
    for (idx, item) in requests.iter().enumerate() {
        if matches!(
            item.mode,
            ContextMode::Search | ContextMode::GrepContext | ContextMode::ExperimentOutputs
        ) {
            return None;
        }
        script.push_str(&format!(" printf '\n__PDCTX_{}_START_{}__\n';", nonce, idx));
        match item.mode {
            ContextMode::Overview => {
                let file_args = [
                    "README.md",
                    "TODO.md",
                    "Cargo.toml",
                    "scripts/e2e_test.sh",
                    "src/main.rs",
                ]
                .iter()
                .map(|f| shell_escape(f))
                .collect::<Vec<_>>()
                .join(" ");
                script.push_str(&format!(" printf '__BRANCH__\\n'; git rev-parse --abbrev-ref HEAD 2>/dev/null || printf 'unknown\\n'; printf '__STATUS__\\n'; git status --short --untracked-files=no 2>/dev/null || true; printf '__FILES__\\n'; for f in {}; do if test -f \"$f\"; then printf '%s=yes\\n' \"$f\"; else printf '%s=no\\n' \"$f\"; fi; done;", file_args));
            }
            ContextMode::Tree => {
                let mut excludes = String::new();
                for dir in IGNORED_DIRS {
                    excludes.push_str(&format!(" -not -path '*/{}/*'", dir));
                }
                let limit = normalize_tree_limit(item.limit);
                let max_depth = normalize_tree_depth(item.max_depth);
                let find_root = match &item.path {
                    Some(path) => {
                        if validate_ssh_read_path(path).is_err() {
                            return None;
                        }
                        shell_escape(path)
                    }
                    None => shell_escape("."),
                };
                script.push_str(&format!(" find {} -mindepth 1 -maxdepth {}{} -type f -print 2>/dev/null | sort | head -n {} | sed 's|^\\./||';", find_root, max_depth, excludes, limit));
            }
            ContextMode::ReadFile => {
                let Some(path) = &item.path else {
                    return None;
                };
                if validate_ssh_read_path(path).is_err() {
                    return None;
                }
                let end_line = match validate_read_file_range(item.start_line, item.limit) {
                    Ok(end_line) => end_line,
                    Err(_) => return None,
                };
                let escaped_path = shell_escape(path);
                script.push_str(&format!(" if test -f {0}; then sed -n '{1},{2}p' -- {0} | awk '{{ if(length($0)>{3}) print substr($0,1,{3}) \"… [line truncated]\"; else print }}'; else printf '__PDCTX_ERROR__:File not found: {0}\\n'; fi;", escaped_path, item.start_line, end_line, MAX_CONTEXT_LINE_LEN));
            }
            ContextMode::MarkdownOutline => {
                let Some(path) = &item.path else {
                    return None;
                };
                if validate_ssh_read_path(path).is_err() {
                    return None;
                }
                script.push_str(&markdown_outline_shell_fragment(path, item.limit));
            }
            ContextMode::ReadSection => {
                let (Some(path), Some(query)) = (&item.path, &item.query) else {
                    return None;
                };
                if validate_ssh_read_path(path).is_err() {
                    return None;
                }
                script.push_str(&markdown_section_shell_fragment(path, query, item.limit));
            }
            ContextMode::AgentContext => {
                script.push_str(&agent_context_shell_fragment());
            }
            ContextMode::GitStatus => {
                script.push_str(" git status --short --untracked-files=no 2>/dev/null || true;");
            }
            ContextMode::GitDiff => {
                script.push_str(" git diff 2>/dev/null || true;");
            }
            ContextMode::Search | ContextMode::GrepContext | ContextMode::ExperimentOutputs => {
                return None
            }
        }
        script.push_str(&format!(" printf '\n__PDCTX_{}_END_{}__\n';", nonce, idx));
    }

    let (code, stdout, stderr, _) = run_ssh_targets(&ssh_targets, &script, 30, ssh_config);
    if code != 0 {
        let error = format!("SSH context batch failed: {}", stderr.trim());
        return Some((
            ssh_context_batch_error_results(project_name, requests, error),
            1,
        ));
    }
    let blocks = parse_ssh_batch_blocks(&stdout, requests.len(), &nonce);
    let results = requests
        .iter()
        .zip(blocks.iter())
        .map(|(item, block)| ssh_batch_block_to_response(proj, project_name, item, block))
        .collect();
    Some((results, 1))
}

// =============================================================================
// Trusted async shell job helpers
// =============================================================================

// =============================================================================
// Handlers
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_ssh_path_rejects_absolute() {
        assert!(validate_ssh_read_path("/etc/passwd").is_err());
    }

    #[test]
    fn test_validate_ssh_path_rejects_traversal() {
        assert!(validate_ssh_read_path("../evil.txt").is_err());
        assert!(validate_ssh_read_path("src/../../../etc/passwd").is_err());
    }

    #[test]
    fn test_validate_ssh_path_rejects_sensitive() {
        assert!(validate_ssh_read_path(".env").is_err());
        assert!(validate_ssh_read_path("secret.pem").is_err());
        assert!(validate_ssh_read_path(".git/config").is_err());
        assert!(validate_ssh_read_path("target/debug/binary").is_err());
        assert!(validate_ssh_read_path("node_modules/pkg/index.js").is_err());
    }

    #[test]
    fn test_validate_ssh_path_allows_normal() {
        assert!(validate_ssh_read_path("src/main.rs").is_ok());
        assert!(validate_ssh_read_path("README.md").is_ok());
        assert!(validate_ssh_read_path("src/lib/helper.rs").is_ok());
    }

    #[test]
    fn test_is_sensitive_path_variants() {
        assert!(is_sensitive_path(".env"));
        assert!(is_sensitive_path(".env.local"));
        assert!(is_sensitive_path("secret.pem"));
        assert!(is_sensitive_path("id_rsa"));
        assert!(is_sensitive_path(".git/config"));
        assert!(!is_sensitive_path("src/main.rs"));
        assert!(!is_sensitive_path("README.md"));
    }

    #[test]
    fn test_validate_command_name_accepts_safe_ids() {
        assert!(validate_command_name("clippy").is_ok());
        assert!(validate_command_name("doc.build-1").is_ok());
    }

    #[test]
    fn test_validate_command_name_rejects_shell_like_text() {
        assert!(validate_command_name("").is_err());
        assert!(validate_command_name("cargo test").is_err());
        assert!(validate_command_name("test;rm").is_err());
        assert!(validate_command_name(&"a".repeat(101)).is_err());
    }

    #[test]
    fn test_get_project_command_returns_configured_command() {
        let mut commands = HashMap::new();
        commands.insert("smoke".to_string(), "echo ok".to_string());
        let proj = ProjectConfig {
            path: "/tmp/project".to_string(),
            executor: crate::projects::Executor::Local,
            host: None,
            ssh_hosts: Vec::new(),
            user: None,
            allow_patch: true,
            allow_command_requests: true,
            allow_raw_command_requests: true,
            default_apply_patch_backend: None,
            allowed_checks: vec![],
            checks: None,
            commands,
        };
        assert_eq!(get_project_command(&proj, "smoke").unwrap(), "echo ok");
        assert!(get_project_command(&proj, "missing").is_err());
    }

    #[test]
    fn test_git_command_status_and_diff_are_fixed() {
        let status = GitRequest {
            project: "p".to_string(),
            operation: GitOperation::Status,
            paths: vec![],
            message: None,
        };
        assert_eq!(
            git_command_for_request(&status).unwrap(),
            "git status --short"
        );
        let diff = GitRequest {
            project: "p".to_string(),
            operation: GitOperation::Diff,
            paths: vec!["src/main.rs".to_string()],
            message: None,
        };
        assert_eq!(
            git_command_for_request(&diff).unwrap(),
            "git diff -- 'src/main.rs'"
        );
    }

    #[test]
    fn test_git_command_commit_is_fixed_and_no_verify() {
        let request = GitRequest {
            project: "p".to_string(),
            operation: GitOperation::Commit,
            paths: vec!["src/main.rs".to_string()],
            message: Some("Add feature".to_string()),
        };
        let cmd = git_command_for_request(&request).unwrap();
        assert!(cmd.contains("git add -- 'src/main.rs'"));
        assert!(cmd.contains("git diff --cached --quiet -- 'src/main.rs'"));
        assert!(cmd.contains("No staged changes to commit"));
        assert!(cmd.contains("git commit -m 'Add feature' --no-verify"));
    }

    #[test]
    fn test_git_command_commit_rejects_bad_message() {
        let request = GitRequest {
            project: "p".to_string(),
            operation: GitOperation::Commit,
            paths: vec!["src/main.rs".to_string()],
            message: Some("bad\nmessage".to_string()),
        };
        assert!(git_command_for_request(&request).is_err());
    }

    #[test]
    fn test_raw_command_validation_rejects_high_risk_tokens() {
        assert!(validate_raw_command_text("echo ok").is_ok());
        assert!(validate_raw_command_text("git status --short").is_ok());
        assert!(validate_raw_command_text("git push origin main").is_err());
        assert!(validate_raw_command_text("sudo systemctl restart nginx").is_err());
        assert!(validate_raw_command_text("rm -rf target").is_err());
        assert!(validate_raw_command_text("echo one\necho two").is_err());
    }

    #[test]
    fn test_git_command_amend_is_fixed_and_no_verify() {
        let request = GitRequest {
            project: "p".to_string(),
            operation: GitOperation::CommitAmendNoEdit,
            paths: vec!["src/codex.rs".to_string()],
            message: None,
        };
        let cmd = git_command_for_request(&request).unwrap();
        assert!(cmd.contains("git add -- 'src/codex.rs'"));
        assert!(cmd.contains("git diff --cached --quiet -- 'src/codex.rs'"));
        assert!(cmd.contains("No staged changes to amend"));
        assert!(cmd.contains("git commit --amend --no-edit --no-verify"));
    }

    #[test]
    fn test_git_paths_reject_too_many_paths() {
        let paths = (0..=MAX_GIT_PATHS)
            .map(|i| format!("src/file{i}.rs"))
            .collect::<Vec<_>>();
        let err = validate_git_paths(&paths).unwrap_err();
        assert!(err.contains("too many paths"));
        assert!(err.contains("50"));
    }

    #[test]
    fn test_git_paths_reject_too_long_path() {
        let long_path = format!("src/{}.rs", "a".repeat(MAX_GIT_PATH_LEN));
        let err = validate_git_paths(&[long_path]).unwrap_err();
        assert!(err.contains("path is too long"));
        assert!(err.contains("512"));
    }

    #[test]
    fn test_git_command_rejects_sensitive_paths() {
        let request = GitRequest {
            project: "p".to_string(),
            operation: GitOperation::Add,
            paths: vec![".env".to_string()],
            message: None,
        };
        assert!(git_command_for_request(&request).is_err());
    }

    #[test]
    fn test_git_mutating_commands_require_paths() {
        let request = GitRequest {
            project: "p".to_string(),
            operation: GitOperation::CommitAmendNoEdit,
            paths: vec![],
            message: None,
        };
        assert!(git_command_for_request(&request).is_err());
    }

    #[test]
    fn test_parse_ssh_batch_blocks_with_nonce() {
        let nonce = "abc123";
        let stdout = "__PDCTX_abc123_START_0__\nfirst\n__PDCTX_abc123_END_0__\n__PDCTX_abc123_START_1__\nsecond\n__PDCTX_abc123_END_1__\n";
        let blocks = parse_ssh_batch_blocks(stdout, 2, nonce);
        assert_eq!(blocks[0], "first\n");
        assert_eq!(blocks[1], "second\n");
    }

    #[test]
    fn test_parse_ssh_batch_blocks_ignores_old_style_markers() {
        let nonce = "abc123";
        let stdout = "__PDCTX_abc123_START_0__\nline before\n__PDCTX_START_0__\nfile content\n__PDCTX_END_0__\nline after\n__PDCTX_abc123_END_0__\n";
        let blocks = parse_ssh_batch_blocks(stdout, 1, nonce);
        assert!(blocks[0].contains("__PDCTX_START_0__"));
        assert!(blocks[0].contains("__PDCTX_END_0__"));
        assert!(blocks[0].contains("line after"));
    }

    #[test]
    fn test_invalid_read_file_ranges_return_errors() {
        assert!(validate_read_file_range(0, 10).is_err());
        assert!(validate_read_file_range(1, 0).is_err());
        assert!(validate_read_file_range(1, MAX_READ_FILE_LIMIT + 1).is_err());
        assert!(validate_read_file_range(usize::MAX, 2).is_err());
    }

    #[test]
    fn test_ssh_batch_failure_returns_one_result_per_request() {
        let requests = vec![
            ContextBatchItem {
                mode: ContextMode::Overview,
                path: None,
                query: None,
                start_line: 1,
                limit: 10,
                max_depth: default_tree_max_depth(),
            },
            ContextBatchItem {
                mode: ContextMode::ReadFile,
                path: Some("README.md".to_string()),
                query: None,
                start_line: 1,
                limit: 10,
                max_depth: default_tree_max_depth(),
            },
        ];
        let results = ssh_context_batch_error_results("proj", &requests, "boom".to_string());
        assert_eq!(results.len(), requests.len());
        assert!(results.iter().all(|r| !r.success));
        assert!(results.iter().all(|r| r.error.as_deref() == Some("boom")));
    }
    #[test]
    fn test_build_ssh_targets() {
        let proj = ProjectConfig {
            path: "/tmp/test".to_string(),
            executor: crate::projects::Executor::Ssh,
            host: Some("msi".to_string()),
            ssh_hosts: vec!["msi-rev4".to_string(), "msi-rev6".to_string()],
            user: Some("root".to_string()),
            allow_patch: false,
            allow_command_requests: false,
            allow_raw_command_requests: false,
            default_apply_patch_backend: None,
            allowed_checks: vec![],
            checks: None,
            commands: HashMap::new(),
        };
        assert_eq!(
            proj.ssh_targets(),
            vec![
                "root@msi".to_string(),
                "root@msi-rev4".to_string(),
                "root@msi-rev6".to_string()
            ]
        );

        let proj_no_user = ProjectConfig {
            path: "/tmp/test".to_string(),
            executor: crate::projects::Executor::Ssh,
            host: Some("msi".to_string()),
            ssh_hosts: vec!["msi".to_string(), "msi-rev6".to_string()],
            user: None,
            allow_patch: false,
            allow_command_requests: false,
            allow_raw_command_requests: false,
            default_apply_patch_backend: None,
            allowed_checks: vec![],
            checks: None,
            commands: HashMap::new(),
        };
        assert_eq!(
            proj_no_user.ssh_targets(),
            vec!["msi".to_string(), "msi-rev6".to_string()]
        );
    }

    #[test]
    fn test_build_ssh_targets_no_host() {
        let proj = ProjectConfig {
            path: "/tmp/test".to_string(),
            executor: crate::projects::Executor::Ssh,
            host: None,
            ssh_hosts: vec![],
            user: None,
            allow_patch: false,
            allow_command_requests: false,
            allow_raw_command_requests: false,
            default_apply_patch_backend: None,
            allowed_checks: vec![],
            checks: None,
            commands: HashMap::new(),
        };
        assert!(proj.ssh_targets().is_empty());
    }

    #[test]
    fn test_local_executor_is_default() {
        let proj = ProjectConfig {
            path: "/tmp/test".to_string(),
            executor: crate::projects::Executor::default(),
            host: None,
            ssh_hosts: Vec::new(),
            user: None,
            allow_patch: false,
            allow_command_requests: false,
            allow_raw_command_requests: false,
            default_apply_patch_backend: None,
            allowed_checks: vec![],
            checks: None,
            commands: HashMap::new(),
        };
        assert!(!proj.is_ssh());
    }

    // =========================================================================
    // Edit unit tests
    // =========================================================================

    #[test]
    fn test_replace_nth_single_match() {
        let result = replace_nth("hello world", "world", "rust", None).unwrap();
        assert_eq!(result, "hello rust");
    }

    #[test]
    fn test_replace_nth_no_match() {
        let result = replace_nth("hello world", "xyz", "abc", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_replace_nth_empty_old() {
        let result = replace_nth("hello", "", "x", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_replace_nth_multiple_no_occurrence() {
        let result = replace_nth("aXbXc", "X", "Y", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("2 times"));
    }

    #[test]
    fn test_replace_nth_multiple_with_occurrence() {
        let result = replace_nth("aXbXc", "X", "Y", Some(2)).unwrap();
        assert_eq!(result, "aXbYc");
    }

    #[test]
    fn test_replace_nth_occurrence_zero() {
        let result = replace_nth("abc", "a", "b", Some(0));
        assert!(result.is_err());
    }

    #[test]
    fn test_replace_nth_occurrence_too_large() {
        let result = replace_nth("abc", "a", "b", Some(5));
        assert!(result.is_err());
    }

    #[test]
    fn test_replace_line_range_basic() {
        let content = "line1\nline2\nline3\n";
        let result = replace_line_range(content, 2, 2, "new2\n").unwrap();
        assert_eq!(result, "line1\nnew2\nline3\n");
    }

    #[test]
    fn test_replace_line_range_multi() {
        let content = "line1\nline2\nline3\nline4\n";
        let result = replace_line_range(content, 2, 3, "replaced\n").unwrap();
        assert_eq!(result, "line1\nreplaced\nline4\n");
    }

    #[test]
    fn test_replace_line_range_invalid_start() {
        let content = "line1\nline2\n";
        let result = replace_line_range(content, 0, 1, "x");
        assert!(result.is_err());
    }

    #[test]
    fn test_replace_line_range_exceeds() {
        let content = "line1\n";
        let result = replace_line_range(content, 1, 5, "x");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_no_mixed_edit_kinds_rejects_same_path_text_binary() {
        let edits = vec![
            EditOperation::WriteFile {
                path: "docs/diagram.bin".to_string(),
                content: "text".to_string(),
                allow_overwrite: true,
            },
            EditOperation::WriteBinaryFile {
                path: "docs/diagram.bin".to_string(),
                base64_content: "AAE=".to_string(),
                allow_overwrite: true,
            },
        ];
        let err = validate_no_mixed_edit_kinds(&edits).unwrap_err();
        assert!(err.contains("cannot mix text and binary edits for the same path"));
    }

    #[test]
    fn test_validate_no_mixed_edit_kinds_allows_same_path_same_kind() {
        let edits = vec![
            EditOperation::WriteBinaryFile {
                path: "docs/diagram.bin".to_string(),
                base64_content: "AAE=".to_string(),
                allow_overwrite: true,
            },
            EditOperation::WriteBinaryFile {
                path: "docs/diagram.bin".to_string(),
                base64_content: "AQI=".to_string(),
                allow_overwrite: true,
            },
        ];
        assert!(validate_no_mixed_edit_kinds(&edits).is_ok());
    }

    #[test]
    fn test_read_binary_from_upload_accepts_project_relative_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("upload.bin");
        std::fs::write(&source, [1_u8, 2, 3, 4]).unwrap();
        let bytes = read_binary_from_upload(dir.path(), "upload.bin", "docs/out.bin").unwrap();
        assert_eq!(bytes, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_read_binary_from_upload_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_binary_from_upload(dir.path(), "../secret.bin", "docs/out.bin").unwrap_err();
        assert!(err.contains("traversal"));
    }

    #[test]
    fn test_validate_source_url_rejects_localhost() {
        let err = validate_source_url("http://localhost:8080/file.png").unwrap_err();
        assert!(err.contains("not allowed"));
        let err = validate_source_url("http://127.0.0.1/file.png").unwrap_err();
        assert!(err.contains("blocked private/local"));
    }

    #[test]
    fn test_validate_source_url_rejects_non_http() {
        let err = validate_source_url("file:///tmp/file.png").unwrap_err();
        assert!(err.contains("http or https"));
    }

    #[test]
    fn test_validate_source_url_allows_chatgpt_estuary_content() {
        let url = validate_source_url("https://chatgpt.com/backend-api/estuary/content?id=file_abc123&ts=1&p=fsns&cid=1&sig=abc&v=0").unwrap();
        assert_eq!(url.host_str(), Some("chatgpt.com"));
        assert_eq!(url.path(), "/backend-api/estuary/content");
    }

    #[test]
    fn test_chatgpt_estuary_allowlist_rejects_non_estuary_path() {
        let url = reqwest::Url::parse("https://chatgpt.com/api/not-estuary?id=file_abc123&sig=abc")
            .unwrap();
        assert!(!is_allowed_chatgpt_estuary_url(&url));
    }

    #[test]
    fn test_decode_binary_artifact_accepts_small_base64() {
        let bytes = decode_binary_artifact("AAECAw==", "docs/pixel.bin").unwrap();
        assert_eq!(bytes, vec![0, 1, 2, 3]);
    }

    #[test]
    fn test_decode_binary_artifact_rejects_invalid_base64() {
        let err = decode_binary_artifact("not valid base64!", "docs/pixel.bin").unwrap_err();
        assert!(err.contains("Invalid base64"));
    }

    #[test]
    fn test_simple_binary_diff_mentions_sizes() {
        let diff = simple_binary_diff("docs/pixel.bin", Some(2), 4);
        assert!(diff.contains("Binary files"));
        assert!(diff.contains("old size: 2"));
        assert!(diff.contains("new size: 4"));
    }

    #[test]
    fn test_validate_edit_path_rejects_env() {
        assert!(validate_edit_path(".env").is_err());
        assert!(validate_edit_path("config/.env").is_err());
    }

    #[test]
    fn test_validate_edit_path_rejects_traversal() {
        assert!(validate_edit_path("../evil.txt").is_err());
        assert!(validate_edit_path("src/../../etc/passwd").is_err());
    }

    #[test]
    fn test_validate_edit_path_rejects_absolute() {
        assert!(validate_edit_path("/etc/passwd").is_err());
    }

    #[test]
    fn test_validate_edit_path_rejects_target() {
        assert!(validate_edit_path("target/debug/binary").is_err());
    }

    #[test]
    fn test_validate_edit_path_allows_normal() {
        assert!(validate_edit_path("src/main.rs").is_ok());
        assert!(validate_edit_path("README.md").is_ok());
        assert!(validate_edit_path(".gitignore").is_ok());
    }

    #[test]
    fn test_validate_edit_path_rejects_git_dir() {
        assert!(validate_edit_path(".git/config").is_err());
        assert!(validate_edit_path(".git/hooks/pre-commit").is_err());
    }

    // =========================================================================
    // SSH edit safety tests
    // =========================================================================

    #[test]
    fn test_shell_escape_no_injection() {
        // Verify that shell_escape properly wraps in single quotes
        // Input: '; rm -rf /; echo '
        // Expected output: '\'''; rm -rf /; echo '\''
        // The outer single quotes prevent shell interpretation of the content
        let dangerous = "'; rm -rf /; echo '";
        let escaped = shell_escape(dangerous);
        // Should start and end with single quote
        assert!(escaped.starts_with('\''));
        assert!(escaped.ends_with('\''));
        // Should contain the escaped single-quote sequence ('\'' means: end quote, literal quote, start quote)
        assert!(escaped.contains("'\\''"));
        // The escaped form should be: '\''  '; rm -rf /; echo '  '\''
        // which is safe because the dangerous content is inside single quotes
    }

    #[test]
    fn test_ssh_edit_command_no_user_input_in_shell() {
        // Verify that user-controlled edit content does not appear in the SSH command string
        let user_input = "'; malicious_command; echo '";
        let body = EditRequest {
            project: "test".to_string(),
            reason: None,
            dry_run: false,
            response_mode: None,
            edits: vec![EditOperation::ReplaceText {
                path: "src/main.rs".to_string(),
                old_text: user_input.to_string(),
                new_text: "safe".to_string(),
                occurrence: None,
            }],
        };
        let _body_json = serde_json::to_string(&body).unwrap();
        // The JSON-serialized body should contain the user input escaped inside JSON,
        // but the shell_escape of the python script itself should not contain raw user input
        let escaped_script = shell_escape(REMOTE_EDIT_SCRIPT);
        assert!(!escaped_script.contains(user_input));
        // The body JSON is piped via stdin, not embedded in the command
        // So the SSH command is: ssh target -- python3 -c '<script>' '<project_path>'
        // Neither argument contains the user's edit payload directly
    }

    // =========================================================================
    // Remote python3 script local run test
    // =========================================================================

    #[test]
    fn test_remote_edit_script_replace_text_local() {
        // Run the embedded python3 script locally to verify it works
        let tmp = tempfile::tempdir().unwrap_or_else(|_| {
            // fallback if tempfile not available
            let d = std::path::PathBuf::from("/tmp/private-drop-test-script");
            let _ = std::fs::create_dir_all(&d);
            // Return a wrapper
            tempfile::TempDir::new_in(&d).unwrap()
        });
        let root = tmp.path();
        std::fs::write(root.join("test.txt"), "hello world\n").unwrap();

        let request = serde_json::json!({
            "dry_run": false,
            "edits": [{
                "type": "replace_text",
                "path": "test.txt",
                "old_text": "world",
                "new_text": "rust"
            }]
        });

        let mut child = std::process::Command::new("python3")
            .arg("-c")
            .arg(REMOTE_EDIT_SCRIPT)
            .arg(root.to_str().unwrap())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("Failed to spawn python3");

        if let Some(ref mut stdin) = child.stdin {
            use std::io::Write;
            stdin.write_all(request.to_string().as_bytes()).unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "Script failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(result["changed_files"][0], "test.txt");
        assert!(result["diff"].as_str().unwrap().contains("-hello world"));
        assert!(result["diff"].as_str().unwrap().contains("+hello rust"));
        // Verify the file was actually modified
        let content = std::fs::read_to_string(root.join("test.txt")).unwrap();
        assert_eq!(content, "hello rust\n");
    }

    #[test]
    fn test_remote_edit_script_dry_run_local() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap_or_else(|_| {
            let d = std::path::PathBuf::from("/tmp/private-drop-test-dry");
            let _ = std::fs::create_dir_all(&d);
            tempfile::TempDir::new_in(&d).unwrap()
        });
        let root = tmp.path();
        std::fs::write(root.join("test.txt"), "original content\n").unwrap();

        let request = serde_json::json!({
            "dry_run": true,
            "edits": [{
                "type": "replace_text",
                "path": "test.txt",
                "old_text": "original",
                "new_text": "changed"
            }]
        });

        let mut child = std::process::Command::new("python3")
            .arg("-c")
            .arg(REMOTE_EDIT_SCRIPT)
            .arg(root.to_str().unwrap())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("Failed to spawn python3");

        if let Some(ref mut stdin) = child.stdin {
            stdin.write_all(request.to_string().as_bytes()).unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["success"], true);
        assert!(result["diff"].as_str().unwrap().contains("-original"));
        assert!(result["diff"].as_str().unwrap().contains("+changed"));
        // Verify the file was NOT modified (dry_run)
        let content = std::fs::read_to_string(root.join("test.txt")).unwrap();
        assert_eq!(content, "original content\n");
    }

    #[test]
    fn test_remote_edit_script_rejects_env() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap_or_else(|_| {
            let d = std::path::PathBuf::from("/tmp/private-drop-test-env");
            let _ = std::fs::create_dir_all(&d);
            tempfile::TempDir::new_in(&d).unwrap()
        });
        let root = tmp.path();

        let request = serde_json::json!({
            "dry_run": false,
            "edits": [{
                "type": "replace_text",
                "path": ".env",
                "old_text": "x",
                "new_text": "y"
            }]
        });

        let mut child = std::process::Command::new("python3")
            .arg("-c")
            .arg(REMOTE_EDIT_SCRIPT)
            .arg(root.to_str().unwrap())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("Failed to spawn python3");

        if let Some(ref mut stdin) = child.stdin {
            stdin.write_all(request.to_string().as_bytes()).unwrap();
        }
        let output = child.wait_with_output().unwrap();
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["success"], false);
        assert!(result["error"].as_str().unwrap().contains("sensitive"));
    }
}

pub(super) fn apply_edit_request_with_metrics(
    projects: &ProjectsConfig,
    proj: &ProjectConfig,
    body: &EditRequest,
    operation: &'static str,
) -> EditResponse {
    let edit_start = Instant::now();
    if proj.is_ssh() {
        let response = ssh_apply_project_edit(proj, body, projects.ssh.as_ref());
        tracing::info!(
            target: "codex.metrics",
            operation = operation,
            project = %body.project,
            executor = "ssh",
            success = response.success,
            dry_run = body.dry_run,
            edit_count = body.edits.len(),
            changed_files = response.changed_files.len(),
            duration_ms = edit_start.elapsed().as_millis() as u64,
            ssh_calls = 1,
            control_master = projects.ssh.as_ref().map(|s| s.control_master).unwrap_or(false),
            "codex_edit_completed"
        );
        return response;
    }
    let response = local_apply_project_edit(proj, body);
    tracing::info!(
        target: "codex.metrics",
        operation = operation,
        project = %body.project,
        executor = "local",
        success = response.success,
        dry_run = body.dry_run,
        edit_count = body.edits.len(),
        changed_files = response.changed_files.len(),
        duration_ms = edit_start.elapsed().as_millis() as u64,
        ssh_calls = 0,
        control_master = false,
        "codex_edit_completed"
    );
    response
}

#[cfg(test)]
mod ssh_command_tests {
    use super::*;

    fn ssh_config() -> SshConfig {
        SshConfig {
            batch_mode: false,
            connect_timeout_secs: None,
            control_master: false,
            control_persist: None,
            control_path: None,
            server_alive_interval: None,
            server_alive_count_max: None,
        }
    }

    fn command_args(command: &std::process::Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn default_ssh_config_does_not_add_controlmaster() {
        let args = ssh_option_args(None);
        assert!(!args.iter().any(|arg| arg.contains("ControlMaster")));
        assert!(args.is_empty());
    }

    #[test]
    fn control_master_adds_reuse_options() {
        let mut cfg = ssh_config();
        cfg.control_master = true;
        cfg.control_persist = Some("10m".into());
        cfg.control_path = Some("/tmp/private-drop-ssh-%C".into());
        let args = ssh_option_args(Some(&cfg));
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(args.contains(&"ControlMaster=auto".to_string()));
        assert!(args.contains(&"ControlPersist=10m".to_string()));
        assert!(args.contains(&"ControlPath=/tmp/private-drop-ssh-%C".to_string()));
    }

    #[test]
    fn batch_mode_without_control_master_adds_batchmode_only() {
        let mut cfg = ssh_config();
        cfg.batch_mode = true;
        let args = ssh_option_args(Some(&cfg));
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(!args.iter().any(|arg| arg.contains("ControlMaster")));
    }

    #[test]
    fn connect_timeout_and_keepalive_options_are_rendered() {
        let mut cfg = ssh_config();
        cfg.connect_timeout_secs = Some(10);
        cfg.server_alive_interval = Some(30);
        cfg.server_alive_count_max = Some(3);
        let args = ssh_option_args(Some(&cfg));
        assert!(args.contains(&"ConnectTimeout=10".to_string()));
        assert!(args.contains(&"ServerAliveInterval=30".to_string()));
        assert!(args.contains(&"ServerAliveCountMax=3".to_string()));
    }

    #[test]
    fn ssh_command_uses_args_not_local_shell() {
        let mut cfg = ssh_config();
        cfg.batch_mode = true;
        let command = build_ssh_command("root@example", "cd /repo && git status", Some(&cfg));
        assert_eq!(command.get_program().to_string_lossy(), "ssh");
        let args = command_args(&command);
        assert_eq!(
            args.last().map(String::as_str),
            Some("cd /repo && git status")
        );
        assert!(args.contains(&"root@example".to_string()));
        assert!(!args.iter().any(|arg| arg == "sh" || arg == "-c"));
    }

    // --- Context batch preflight tests ---

    fn make_batch_request(
        items: Vec<ContextBatchItem>,
        max_total_chars: usize,
    ) -> ContextBatchRequest {
        ContextBatchRequest {
            project: "test".to_string(),
            requests: items,
            max_total_chars,
        }
    }

    #[test]
    fn preflight_local_small_batch_passes() {
        let req = make_batch_request(
            vec![
                ContextBatchItem {
                    mode: ContextMode::Overview,
                    path: None,
                    query: None,
                    start_line: 1,
                    limit: 200,
                    max_depth: default_tree_max_depth(),
                },
                ContextBatchItem {
                    mode: ContextMode::ReadFile,
                    path: Some("README.md".to_string()),
                    query: None,
                    start_line: 1,
                    limit: 50,
                    max_depth: default_tree_max_depth(),
                },
            ],
            60_000,
        );
        let result = context::preflight_context_batch(&req, false, "test");
        assert!(result.is_ok(), "Small local batch should pass preflight");
    }

    #[test]
    fn preflight_rejects_max_total_chars_over_hard_limit() {
        let req = make_batch_request(
            vec![ContextBatchItem {
                mode: ContextMode::Overview,
                path: None,
                query: None,
                start_line: 1,
                limit: 200,
                max_depth: default_tree_max_depth(),
            }],
            200_000, // exceeds PREFLIGHT_MAX_TOTAL_CHARS (120_000)
        );
        let result = context::preflight_context_batch(&req, false, "test");
        assert!(
            result.is_err(),
            "Should reject max_total_chars over hard limit"
        );
        let resp = result.unwrap_err();
        assert!(!resp.success);
        assert_eq!(resp.preflight_rejected, Some(true));
        assert!(resp.error.as_ref().unwrap().contains("too large"));
        assert!(resp.suggestion.is_some());
        assert!(resp.max_allowed_chars.is_some());
    }

    #[test]
    fn preflight_ssh_rejects_too_many_items() {
        let items: Vec<ContextBatchItem> = (0..10)
            .map(|_| ContextBatchItem {
                mode: ContextMode::ReadFile,
                path: Some("file.txt".to_string()),
                query: None,
                start_line: 1,
                limit: 50,
                max_depth: default_tree_max_depth(),
            })
            .collect();
        let req = make_batch_request(items, 60_000);
        let result = context::preflight_context_batch(&req, true, "test");
        assert!(result.is_err(), "Should reject SSH batch with >8 items");
        let resp = result.unwrap_err();
        assert!(!resp.success);
        assert_eq!(resp.preflight_rejected, Some(true));
        assert_eq!(resp.project_is_ssh, Some(true));
        assert!(resp.max_allowed_items.is_some());
        assert!(resp.suggestion.as_ref().unwrap().contains("SSH"));
    }

    #[test]
    fn preflight_ssh_small_batch_passes() {
        let items: Vec<ContextBatchItem> = (0..6)
            .map(|_| ContextBatchItem {
                mode: ContextMode::ReadFile,
                path: Some("file.txt".to_string()),
                query: None,
                start_line: 1,
                limit: 50,
                max_depth: default_tree_max_depth(),
            })
            .collect();
        let req = make_batch_request(items, 60_000);
        let result = context::preflight_context_batch(&req, true, "test");
        assert!(
            result.is_ok(),
            "SSH batch with 6 items should pass preflight"
        );
    }

    #[test]
    fn preflight_rejects_large_read_file_limit_on_ssh() {
        let req = make_batch_request(
            vec![ContextBatchItem {
                mode: ContextMode::ReadFile,
                path: Some("big.rs".to_string()),
                query: None,
                start_line: 1,
                limit: 800, // exceeds PREFLIGHT_MAX_READ_FILE_LIMIT (400)
                max_depth: default_tree_max_depth(),
            }],
            60_000,
        );
        let result = context::preflight_context_batch(&req, true, "test");
        assert!(
            result.is_err(),
            "Should reject SSH read_file with limit > 400"
        );
        let resp = result.unwrap_err();
        assert!(resp.error.as_ref().unwrap().contains("read_file limit"));
    }

    #[test]
    fn preflight_rejects_git_diff_plus_many_reads() {
        // git_diff estimates 40k, 5 read_file(limit=400) each estimates 48k = 240k total
        let mut items = vec![ContextBatchItem {
            mode: ContextMode::GitDiff,
            path: None,
            query: None,
            start_line: 1,
            limit: 200,
            max_depth: default_tree_max_depth(),
        }];
        for _ in 0..5 {
            items.push(ContextBatchItem {
                mode: ContextMode::ReadFile,
                path: Some("file.rs".to_string()),
                query: None,
                start_line: 1,
                limit: 400,
                max_depth: default_tree_max_depth(),
            });
        }
        // max_total_chars = 60k but estimate ≈ 40k + 5*48k = 280k → 3x budget
        let req = make_batch_request(items, 60_000);
        let result = context::preflight_context_batch(&req, false, "test");
        assert!(
            result.is_err(),
            "Should reject git_diff + many read_file as too large"
        );
        let resp = result.unwrap_err();
        assert!(resp.estimated_chars.is_some());
        assert!(resp.suggestion.is_some());
        assert!(resp.suggestion.as_ref().unwrap().contains("Split"));
    }

    #[test]
    fn preflight_rejection_contains_suggestion() {
        let req = make_batch_request(
            vec![ContextBatchItem {
                mode: ContextMode::Overview,
                path: None,
                query: None,
                start_line: 1,
                limit: 200,
                max_depth: default_tree_max_depth(),
            }],
            200_000,
        );
        let result = context::preflight_context_batch(&req, true, "test");
        let resp = result.unwrap_err();
        assert!(resp.suggestion.is_some());
        assert!(!resp.suggestion.as_ref().unwrap().is_empty());
        assert_eq!(resp.preflight_rejected, Some(true));
        assert!(resp.estimated_chars.is_some());
        assert!(resp.max_allowed_chars.is_some());
        assert_eq!(resp.project_is_ssh, Some(true));
    }

    #[test]
    fn preflight_local_large_batch_warns() {
        // 13 items on local → should get a warning but still pass
        let items: Vec<ContextBatchItem> = (0..13)
            .map(|_| ContextBatchItem {
                mode: ContextMode::ReadFile,
                path: Some("f.txt".to_string()),
                query: None,
                start_line: 1,
                limit: 50,
                max_depth: default_tree_max_depth(),
            })
            .collect();
        let req = make_batch_request(items, 120_000);
        let result = context::preflight_context_batch(&req, false, "test");
        assert!(result.is_ok(), "Local 13 items should pass (not SSH)");
        let warnings = result.unwrap();
        assert!(!warnings.is_empty(), "Should have warning about batch size");
        assert!(
            warnings[0].contains("splitting")
                || warnings[0].contains("Splitting")
                || warnings[0].contains("batches")
        );
    }
}

#[cfg(test)]
mod trusted_command_tests {
    use super::trusted::*;
    use super::*;
    use crate::codex::jobs::{
        build_script_job_command, build_trusted_script_content, build_trusted_script_job_command,
        create_local_job,
    };

    fn make_local_proj() -> ProjectConfig {
        ProjectConfig {
            path: std::env::temp_dir().to_string_lossy().to_string(),
            executor: crate::projects::Executor::Local,
            host: None,
            ssh_hosts: Vec::new(),
            user: None,
            allow_patch: true,
            allow_command_requests: true,
            allow_raw_command_requests: true,
            default_apply_patch_backend: None,
            allowed_checks: vec![],
            checks: None,
            commands: HashMap::new(),
        }
    }

    // --- Test 1: create_trusted_raw_and_approve still works for multi-line ---

    #[test]
    fn trusted_raw_multiline_script_executes() {
        let proj = make_local_proj();
        let script = "echo hello\necho world";
        let wrapped = build_trusted_wrapper(script);
        let (code, stdout, stderr, _duration) = run_project_cmd(&proj, &wrapped, 30, None);
        assert_eq!(code, 0, "stderr: {}", stderr);
        assert!(stdout.contains("hello"), "stdout: {}", stdout);
        assert!(stdout.contains("world"), "stdout: {}", stdout);
    }

    #[test]
    fn trusted_raw_cwd_is_project_root() {
        let proj = make_local_proj();
        let script = "pwd";
        let wrapped = build_trusted_wrapper(script);
        let (code, stdout, stderr, _duration) = run_project_cmd(&proj, &wrapped, 30, None);
        assert_eq!(code, 0, "stderr: {}", stderr);
        assert!(
            stdout.contains(&proj.path),
            "stdout should contain project root, got: {}",
            stdout
        );
    }

    // --- Test 2: trusted script command does NOT produce the old broken pattern ---

    #[test]
    fn trusted_script_command_does_not_use_quoted_script() {
        // The OLD broken pattern was: set -euo pipefail; '<escaped_script>'
        // The NEW correct pattern is: bash .codex/jobs/<job_id>/script.sh
        let job_id = "test-job-123";
        let cmd = build_trusted_script_job_command(job_id);
        // Must NOT contain the old pattern of single-quoting the whole script
        assert!(
            !cmd.contains("set -euo pipefail; '"),
            "command should NOT use the old broken pattern, got: {}",
            cmd
        );
        // Must point to the script.sh file
        assert!(
            cmd.contains("script.sh"),
            "command should reference script.sh, got: {}",
            cmd
        );
        assert!(
            cmd.contains(job_id),
            "command should contain job_id, got: {}",
            cmd
        );
        assert!(
            cmd.contains("bash"),
            "command should use bash to execute the script, got: {}",
            cmd
        );
    }

    // --- Test 3: script.sh content includes shebang, set -euo pipefail, and original script ---

    #[test]
    fn trusted_script_content_has_shebang_and_safety() {
        let content = build_trusted_script_content("echo hello\necho world");
        assert!(
            content.starts_with("#!/usr/bin/env bash\n"),
            "script should start with shebang, got: {}",
            content
        );
        assert!(
            content.contains("set -euo pipefail"),
            "script should contain set -euo pipefail, got: {}",
            content
        );
        assert!(
            content.contains("echo hello"),
            "script should contain original script text, got: {}",
            content
        );
        assert!(
            content.contains("echo world"),
            "script should contain original script text, got: {}",
            content
        );
    }

    // --- Test 4: Local trusted script job actually runs and produces output ---

    #[test]
    fn local_trusted_script_job_executes_and_produces_output() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = ProjectConfig {
            path: tmp.path().to_string_lossy().to_string(),
            executor: crate::projects::Executor::Local,
            host: None,
            ssh_hosts: Vec::new(),
            user: None,
            allow_patch: true,
            allow_command_requests: true,
            allow_raw_command_requests: true,
            default_apply_patch_backend: None,
            allowed_checks: vec![],
            checks: None,
            commands: HashMap::new(),
        };
        // Create .codex/jobs dir so the job can be created
        std::fs::create_dir_all(tmp.path().join(".codex/jobs")).unwrap();

        let script_text = "echo hello_from_trusted_job";
        let result = create_local_job(
            &proj,
            "test-project",
            "goal-test",
            "", // placeholder for trusted_script_text mode
            None,
            Some("trusted_script".to_string()),
            None,
            None,
            Some("test reason".to_string()),
            60,
            Some(script_text),
        );
        assert!(result.is_ok(), "job creation should succeed: {:?}", result);
        let job = result.unwrap();
        assert_eq!(job.kind, Some("trusted_script".to_string()));

        // Wait for the job to finish
        let mut attempts = 0;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let dir = proj.root().join(".codex/jobs").join(&job.job_id);
            let status =
                std::fs::read_to_string(dir.join("status")).unwrap_or_else(|_| "running".into());
            if status != "running" || attempts > 50 {
                break;
            }
            attempts += 1;
        }

        // Check that the job produced output
        let dir = proj.root().join(".codex/jobs").join(&job.job_id);
        let stdout = std::fs::read_to_string(dir.join("stdout.log")).unwrap_or_default();
        assert!(
            stdout.contains("hello_from_trusted_job"),
            "stdout should contain script output, got: {}",
            stdout
        );

        // Verify script.sh exists and has proper content
        let script_content = std::fs::read_to_string(dir.join("script.sh")).unwrap_or_default();
        assert!(
            script_content.contains("#!/usr/bin/env bash"),
            "script.sh should have shebang"
        );
        assert!(
            script_content.contains("set -euo pipefail"),
            "script.sh should have set -euo pipefail"
        );
        assert!(
            script_content.contains("echo hello_from_trusted_job"),
            "script.sh should contain original script"
        );

        // Verify command references script.sh
        assert!(
            job.command.contains("script.sh"),
            "job command should reference script.sh, got: {}",
            job.command
        );
    }

    // --- Test 5: script_text without trusted=true is rejected ---
    // (This is tested at the handler level, but we test the validation function)

    #[test]
    fn trusted_raw_stdout_is_truncated() {
        let result = build_trusted_result(
            0,
            100,
            "/tmp",
            &"a".repeat(100_000),
            &"b".repeat(50_000),
            "summary",
            None,
            false,
        );
        assert!(result.stdout_truncated);
        assert!(result.stderr_truncated);
    }

    // --- Test 6: Denylist / secret / background checks still work ---

    #[test]
    fn dangerous_command_blocked_by_denylist() {
        assert!(check_denylist("rm -rf /").is_some());
        assert!(check_denylist("mkfs.ext4 /dev/sda1").is_some());
        assert!(check_denylist("systemctl restart nginx").is_some());
        assert!(check_denylist("git push origin main").is_some());
        assert!(check_denylist("docker system prune -af").is_some());
    }

    #[test]
    fn git_push_blocked_by_denylist() {
        assert!(check_denylist("git push").is_some());
        assert!(check_denylist("git push origin main").is_some());
        assert!(check_denylist("git push --force").is_some());
    }

    #[test]
    fn env_content_read_blocked() {
        assert!(check_secret_read("cat .env").is_some());
        assert!(check_secret_read("cat id_rsa").is_some());
        assert!(check_secret_read("cat server.pem").is_some());
    }

    #[test]
    fn nohup_disown_background_ampersand_rejected() {
        assert!(check_background_escape("nohup python train.py").is_some());
        assert!(check_background_escape("disown %1").is_some());
        assert!(check_background_escape("sleep 100 &").is_some());
    }

    // --- Test 7: Job create response is lightweight ---

    #[test]
    fn job_create_response_is_lightweight() {
        let response = types::JobOpResponse {
            success: true,
            op: "create".to_string(),
            job_id: Some("job-1".to_string()),
            job_ids: vec!["job-1".to_string()],
            job: None,
            jobs: Vec::new(),
            stdout_tail: None,
            stderr_tail: None,
            summary_markdown: None,
            error: None,
            log_total_lines: None,
            next_cursor: None,
            metadata_only: None,
            logs_included: None,
            warnings: Vec::new(),
        };
        assert_eq!(response.stdout_tail, None);
        assert_eq!(response.stderr_tail, None);
        assert_eq!(response.summary_markdown, None);
    }

    // --- Test 8: OpenAPI schema still has trusted fields ---

    #[test]
    fn openapi_schema_contains_trusted_descriptions() {
        let spec: serde_json::Value =
            serde_json::from_str(include_str!("../data/openapi.json")).unwrap();

        let op_enum: Vec<String> = spec["components"]["schemas"]["CommandRequestOpRequest"]
            ["properties"]["op"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            op_enum.contains(&"create_trusted_raw".to_string()),
            "op enum should contain 'create_trusted_raw', got: {:?}",
            op_enum
        );
        assert!(
            op_enum.contains(&"create_trusted_raw_and_approve".to_string()),
            "op enum should contain 'create_trusted_raw_and_approve', got: {:?}",
            op_enum
        );

        let cr_props = &spec["components"]["schemas"]["CommandRequestOpRequest"]["properties"];
        assert!(!cr_props["script_text"].is_null());
        assert!(!cr_props["timeout_secs"].is_null());
        assert!(!cr_props["response_mode"].is_null());

        let job_props = &spec["components"]["schemas"]["JobOpRequest"]["properties"];
        assert!(!job_props["script_text"].is_null());
        assert!(!job_props["trusted"].is_null());

        let resp_props = &spec["components"]["schemas"]["CommandRequestOpResponse"]["properties"];
        assert!(!resp_props["trusted_result"].is_null());
    }

    // --- Test 9: Old create_raw and script_path behavior unchanged ---

    #[test]
    fn old_create_raw_behavior_unchanged() {
        assert!(validate_raw_command_text("echo ok").is_ok());
        assert!(validate_raw_command_text("git status --short").is_ok());
        assert!(validate_raw_command_text("git push").is_err());
        assert!(validate_raw_command_text("sudo rm -rf /").is_err());
        assert!(validate_raw_command_text("echo one\necho two").is_err());
    }

    #[test]
    fn old_run_job_op_script_path_behavior_unchanged() {
        let result = build_script_job_command("scripts/test.sh", &[]);
        assert!(result.is_ok());
        let cmd = result.unwrap();
        assert!(cmd.contains("scripts/test.sh"));
        assert!(cmd.contains("bash"));
    }
}
