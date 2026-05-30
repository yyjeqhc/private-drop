use crate::projects::canonicalize_and_verify;

use super::shell::shell_escape;
use super::types::{ContextBatchItem, ContextMode, ContextResponse};
use std::path::Path;

pub(super) const IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    ".cache",
    "__pycache__",
];
pub(super) const MAX_TREE_ITEMS: usize = 300;
pub(super) const MAX_SEARCH_RESULTS: usize = 50;
pub(super) const MAX_CONTEXT_LINE_LEN: usize = 2_000;
pub(super) const MAX_TREE_DEPTH: usize = 8;
pub(super) const MAX_READ_FILE_LIMIT: usize = 2_000;
const CONTEXT_MAX_OUTPUT_LEN: usize = 50_000;

pub(super) fn truncate_context_line(line: &str) -> (String, bool) {
    if line.len() <= MAX_CONTEXT_LINE_LEN {
        return (line.to_string(), false);
    }
    let mut end = MAX_CONTEXT_LINE_LEN;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}… [line truncated]", &line[..end]), true)
}

pub(super) fn format_context_line(line_no: usize, line: &str) -> (String, bool) {
    let (line, truncated) = truncate_context_line(line);
    (format!("{:4} | {}", line_no, line), truncated)
}

pub(super) fn git_status_command() -> &'static str {
    "git status --short --untracked-files=no"
}

fn truncate_output_string(s: String, max_len: usize) -> (String, bool) {
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

pub(super) fn normalize_tree_depth(max_depth: usize) -> usize {
    max_depth.clamp(1, MAX_TREE_DEPTH)
}

pub(super) fn normalize_tree_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_TREE_ITEMS)
}

pub(super) fn is_ignored_dir(name: &str) -> bool {
    IGNORED_DIRS.contains(&name) || name.starts_with('.')
}

pub(super) fn collect_tree(
    dir: &Path,
    base: &Path,
    items: &mut Vec<String>,
    limit: usize,
    max_depth: usize,
) {
    if items.len() >= limit || max_depth == 0 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    sorted.sort_by_key(|e| e.file_name());
    for entry in sorted {
        if items.len() >= limit {
            break;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if is_ignored_dir(&name) {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        if path.is_dir() {
            items.push(format!("{}/", rel));
            collect_tree(&path, base, items, limit, max_depth - 1);
        } else {
            items.push(rel);
        }
    }
}

pub(super) fn simple_search(dir: &Path, query: &str, limit: usize) -> Vec<String> {
    let mut results = Vec::new();
    search_recursive(dir, dir, query, &mut results, limit);
    results
}

fn search_recursive(dir: &Path, base: &Path, query: &str, results: &mut Vec<String>, limit: usize) {
    if results.len() >= limit {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.filter_map(|e| e.ok()) {
        if results.len() >= limit {
            return;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if is_ignored_dir(&name) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            search_recursive(&path, base, query, results, limit);
        } else if path.is_file() {
            // Only search text files (skip large files)
            let metadata = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if metadata.len() > 1_000_000 {
                continue;
            } // skip >1MB
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue, // skip binary files
            };
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            for (i, line) in content.lines().enumerate() {
                if results.len() >= limit {
                    return;
                }
                if line.contains(query) {
                    results.push(format!("{}:{}: {}", rel, i + 1, line.trim()));
                }
            }
        }
    }
}

pub(super) fn mode_name(mode: &ContextMode) -> &'static str {
    match mode {
        ContextMode::Overview => "overview",
        ContextMode::Tree => "tree",
        ContextMode::Search => "search",
        ContextMode::ReadFile => "read_file",
        ContextMode::MarkdownOutline => "markdown_outline",
        ContextMode::ReadSection => "read_section",
        ContextMode::AgentContext => "agent_context",
        ContextMode::GitStatus => "git_status",
        ContextMode::GitDiff => "git_diff",
    }
}

pub(super) fn context_error(project: &str, mode: &ContextMode, error: String) -> ContextResponse {
    ContextResponse {
        success: false,
        project: project.to_string(),
        mode: mode_name(mode).to_string(),
        content: None,
        items: None,
        truncated: false,
        error: Some(error),
    }
}

pub(super) fn validate_read_file_range(start_line: usize, limit: usize) -> Result<usize, String> {
    if start_line == 0 {
        return Err("start_line must be >= 1".to_string());
    }
    if limit == 0 {
        return Err("limit must be >= 1".to_string());
    }
    if limit > MAX_READ_FILE_LIMIT {
        return Err(format!("limit must be <= {}", MAX_READ_FILE_LIMIT));
    }
    start_line
        .checked_add(limit - 1)
        .ok_or_else(|| "start_line + limit - 1 overflowed".to_string())
}

pub(super) const AGENT_CONTEXT_FILES: &[&str] = &[
    "AGENTS.md",
    ".codex/memory/project.md",
    ".codex/memory/pitfalls.md",
    ".codex/memory/workflows.md",
    ".codex/memory/decisions.md",
    ".codex/memory/user_preferences.md",
];

pub(super) fn mode_content_response(
    project_name: &str,
    mode: &str,
    content: String,
    max_len: usize,
) -> ContextResponse {
    let (content, truncated) = truncate_output_string(content, max_len);
    ContextResponse {
        success: true,
        project: project_name.to_string(),
        mode: mode.to_string(),
        content: Some(content),
        items: None,
        truncated,
        error: None,
    }
}

pub(super) fn local_agent_context(root: &Path, project_name: &str) -> ContextResponse {
    let mut content = format!(
        "# Agent context for {}\n\nLoaded project rules and memory files for alignment before planning or editing.\n",
        project_name
    );
    for rel in AGENT_CONTEXT_FILES {
        content.push_str(&format!("\n## {}\n\n", rel));
        let path = root.join(rel);
        match canonicalize_and_verify(&path, root) {
            Ok(canonical) => match std::fs::read_to_string(&canonical) {
                Ok(text) => content.push_str(text.trim_end()),
                Err(_) => content.push_str("(missing)"),
            },
            Err(_) => content.push_str("(missing)"),
        }
        content.push('\n');
    }
    mode_content_response(
        project_name,
        "agent_context",
        content,
        CONTEXT_MAX_OUTPUT_LEN,
    )
}

fn markdown_outline_from_text(project_name: &str, text: &str, limit: usize) -> ContextResponse {
    let max = limit.clamp(1, MAX_READ_FILE_LIMIT);
    let mut lines = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed.chars().nth(hashes) == Some(' ') {
            lines.push(format!("{:4} | {}", idx + 1, trimmed));
            if lines.len() >= max {
                break;
            }
        }
    }
    let truncated = lines.len() >= max;
    ContextResponse {
        success: true,
        project: project_name.to_string(),
        mode: "markdown_outline".to_string(),
        content: Some(lines.join("\n")),
        items: None,
        truncated,
        error: None,
    }
}

fn markdown_section_from_text(
    project_name: &str,
    text: &str,
    query: &str,
    limit: usize,
) -> ContextResponse {
    let max = limit.clamp(1, MAX_READ_FILE_LIMIT);
    let query_lower = query.to_lowercase();
    let mut found = false;
    let mut level = 0usize;
    let mut selected = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        let is_heading = (1..=6).contains(&hashes) && trimmed.chars().nth(hashes) == Some(' ');
        if is_heading {
            if found && hashes <= level {
                break;
            }
            if !found && trimmed.to_lowercase().contains(&query_lower) {
                found = true;
                level = hashes;
            }
        }
        if found {
            if selected.len() >= max {
                break;
            }
            selected.push(format!("{:4} | {}", idx + 1, line));
        }
    }
    if !found {
        return ContextResponse {
            success: false,
            project: project_name.to_string(),
            mode: "read_section".to_string(),
            content: None,
            items: None,
            truncated: false,
            error: Some(format!("Section not found: {}", query)),
        };
    }
    let truncated = selected.len() >= max;
    ContextResponse {
        success: true,
        project: project_name.to_string(),
        mode: "read_section".to_string(),
        content: Some(selected.join("\n")),
        items: None,
        truncated,
        error: None,
    }
}

pub(super) fn enforce_context_batch_total_limit(
    results: &mut [ContextResponse],
    max_total_chars: usize,
) {
    let max_total = max_total_chars.clamp(4_000, 200_000);
    let mut used = 0usize;
    for result in results {
        if let Some(content) = result.content.as_mut() {
            if used >= max_total {
                content.clear();
                result.truncated = true;
                continue;
            }
            let remaining = max_total - used;
            if content.len() > remaining {
                let (truncated, _) = truncate_output_string(content.clone(), remaining);
                *content = truncated;
                result.truncated = true;
                used = max_total;
            } else {
                used += content.len();
            }
        }
        if let Some(items) = result.items.as_mut() {
            let mut kept = Vec::new();
            for item in items.iter() {
                if used + item.len() + 1 > max_total {
                    result.truncated = true;
                    break;
                }
                used += item.len() + 1;
                kept.push(item.clone());
            }
            *items = kept;
        }
    }
}

pub(super) fn markdown_outline_shell_fragment(path: &str, limit: usize) -> String {
    format!(
        " if test -f {0}; then grep -n -E '^#{{1,6}}[[:space:]]+' -- {0} | head -n {1}; else printf '__PDCTX_ERROR__:File not found: {0}\\n'; fi;",
        shell_escape(path),
        limit.clamp(1, MAX_READ_FILE_LIMIT)
    )
}

pub(super) fn markdown_section_shell_fragment(path: &str, query: &str, limit: usize) -> String {
    format!(
        " if test -f {path}; then awk -v q={query} -v max={limit} 'BEGIN{{found=0;level=0;count=0}} /^#{{1,6}}[ \\t]+/{{ if(found){{ match($0,/^#+/); if(RLENGTH<=level) exit }} if(!found && index(tolower($0),tolower(q))>0){{ match($0,/^#+/); level=RLENGTH; found=1 }} }} found && count<max {{ printf \"%4d | %s\\n\", NR, $0; count++ }} END{{ if(!found) printf \"__PDCTX_ERROR__:Section not found: %s\\n\", q }}' -- {path}; else printf '__PDCTX_ERROR__:File not found: {path}\\n'; fi;",
        path = shell_escape(path),
        query = shell_escape(query),
        limit = limit.clamp(1, MAX_READ_FILE_LIMIT)
    )
}

pub(super) fn local_markdown_file_response(
    root: &Path,
    project_name: &str,
    item: &ContextBatchItem,
) -> (ContextResponse, u64) {
    let Some(rel_path) = &item.path else {
        return (
            context_error(
                project_name,
                &item.mode,
                "path parameter is required for markdown mode".to_string(),
            ),
            0,
        );
    };
    let full_path = root.join(rel_path);
    match canonicalize_and_verify(&full_path, root) {
        Ok(canonical) => match std::fs::read_to_string(&canonical) {
            Ok(content) => match item.mode {
                ContextMode::MarkdownOutline => (
                    markdown_outline_from_text(project_name, &content, item.limit),
                    0,
                ),
                ContextMode::ReadSection => {
                    let Some(query) = item.query.as_deref() else {
                        return (
                            context_error(
                                project_name,
                                &item.mode,
                                "query parameter is required for read_section mode".to_string(),
                            ),
                            0,
                        );
                    };
                    (
                        markdown_section_from_text(project_name, &content, query, item.limit),
                        0,
                    )
                }
                _ => unreachable!(),
            },
            Err(e) => (
                context_error(
                    project_name,
                    &item.mode,
                    format!("Failed to read file: {}", e),
                ),
                0,
            ),
        },
        Err(e) => (context_error(project_name, &item.mode, e), 0),
    }
}
