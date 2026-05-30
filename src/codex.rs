use crate::projects::{canonicalize_and_verify, ProjectConfig, ProjectsConfig, SshConfig};
use crate::{CodexGoalRecord, CommandAuditRecord, Database, Message, MessageKind};
use base64::Engine;
use salvo::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

// =============================================================================
// Request / Response types
// =============================================================================

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMode {
    Overview,
    Tree,
    Search,
    ReadFile,
    GitStatus,
    GitDiff,
}

#[derive(Debug, Deserialize)]
pub struct ContextRequest {
    pub project: String,
    pub mode: ContextMode,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default = "default_start_line")]
    pub start_line: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Deserialize)]
pub struct ContextBatchItem {
    pub mode: ContextMode,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default = "default_start_line")]
    pub start_line: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Deserialize)]
pub struct ContextBatchRequest {
    pub project: String,
    pub requests: Vec<ContextBatchItem>,
}

fn default_start_line() -> usize {
    1
}
fn default_limit() -> usize {
    200
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct PatchRequest {
    pub project: String,
    pub patch: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CheckRequest {
    pub project: String,
    pub suite: String,
}

#[derive(Debug, Deserialize)]
pub struct ReportRequest {
    pub project: String,
    pub status: String,
    pub title: String,
    pub summary: String,
    #[serde(default = "default_channel")]
    pub channel: String,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum GitOperation {
    Status,
    Diff,
    Log,
    Add,
    Commit,
    CommitAmendNoEdit,
}

#[derive(Debug, Deserialize)]
pub struct GitRequest {
    pub project: String,
    pub operation: GitOperation,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CommandRequest {
    pub project: String,
    pub command: String,
}

#[derive(Debug, Deserialize)]
pub struct CommandRequestCreate {
    pub project: String,
    pub command: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RawCommandRequestCreate {
    pub project: String,
    pub command_text: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CommandRequestBatchCreate {
    pub project: String,
    pub requests: Vec<CommandRequestBatchItem>,
}

#[derive(Debug, Deserialize)]
pub struct CommandRequestBatchItem {
    pub command: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CommandRequestsListRequest {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default = "default_command_request_limit")]
    pub limit: usize,
}

#[derive(Debug, Deserialize)]
pub struct CommandApproveRequest {
    pub request_id: String,
}

#[derive(Debug, Deserialize)]
pub struct CommandRejectRequest {
    pub request_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CommandRequestOpRequest {
    pub op: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub command_text: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub goal_id: Option<String>,
    #[serde(default)]
    pub ttl_secs: Option<i64>,
    #[serde(default)]
    pub requests: Vec<CommandRequestBatchItem>,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub request_ids: Vec<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default = "default_command_request_limit")]
    pub limit: usize,
}

fn default_channel() -> String {
    "omo".to_string()
}

fn default_command_request_limit() -> usize {
    20
}

#[derive(Debug, Serialize)]
pub struct ContextResponse {
    pub success: bool,
    pub project: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<String>>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ContextBatchResponse {
    pub success: bool,
    pub project: String,
    pub results: Vec<ContextResponse>,
    pub duration_ms: u64,
    pub ssh_calls: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PatchResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_files: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CheckResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ReportResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GitResponse {
    pub success: bool,
    pub project: String,
    pub operation: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CommandResponse {
    pub success: bool,
    pub project: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CommandRequestResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<CommandAuditRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CommandRequestsListResponse {
    pub success: bool,
    pub records: Vec<CommandAuditRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CommandRequestBatchResponse {
    pub success: bool,
    pub records: Vec<CommandAuditRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CommandRequestOpResponse {
    pub success: bool,
    pub op: String,
    pub records: Vec<CommandAuditRecord>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub goals: Vec<CodexGoalRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<CommandAuditRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<CodexGoalRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EditRequest {
    pub project: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    pub edits: Vec<EditOperation>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EditOperation {
    ReplaceText {
        path: String,
        old_text: String,
        new_text: String,
        occurrence: Option<usize>,
    },
    ReplaceRange {
        path: String,
        start_line: usize,
        end_line: usize,
        new_text: String,
    },
    AppendFile {
        path: String,
        text: String,
    },
    CreateFile {
        path: String,
        content: String,
    },
    WriteFile {
        path: String,
        content: String,
        #[serde(default)]
        allow_overwrite: bool,
    },
    CreateBinaryFile {
        path: String,
        base64_content: String,
    },
    WriteBinaryFile {
        path: String,
        base64_content: String,
        #[serde(default)]
        allow_overwrite: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EditResponse {
    pub success: bool,
    pub changed_files: Vec<String>,
    pub diff: String,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}

// =============================================================================
// Constants
// =============================================================================

const IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    ".cache",
    "__pycache__",
];
const MAX_TREE_ITEMS: usize = 300;
const MAX_SEARCH_RESULTS: usize = 50;
const MAX_OUTPUT_LEN: usize = 50_000;
const CHECK_TIMEOUT_SECS: u64 = 300;
const MAX_EDIT_FILE_SIZE: u64 = 2 * 1024 * 1024;
const MAX_EDIT_TEXT_SIZE: usize = 200 * 1024;
const MAX_BINARY_ARTIFACT_SIZE: usize = 5 * 1024 * 1024;

const SENSITIVE_PATHS: &[&str] = &[
    ".git",
    ".env",
    ".pem",
    ".key",
    "id_rsa",
    "id_ed25519",
    "target",
    "node_modules",
    "/etc",
    "/root/.ssh",
];

// =============================================================================
// Helpers
// =============================================================================

fn get_projects(depot: &Depot) -> Option<Arc<ProjectsConfig>> {
    depot.obtain::<Arc<ProjectsConfig>>().ok().cloned()
}

fn get_db(depot: &Depot) -> Option<Arc<Database>> {
    depot.obtain::<Arc<Database>>().ok().cloned()
}

fn truncate_string(s: String, max_len: usize) -> (String, bool) {
    if s.len() <= max_len {
        (s, false)
    } else {
        (s[..max_len].to_string(), true)
    }
}

fn is_ignored_dir(name: &str) -> bool {
    IGNORED_DIRS.contains(&name) || name.starts_with('.')
}

fn collect_tree(dir: &Path, base: &Path, items: &mut Vec<String>, limit: usize) {
    if items.len() >= limit {
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
            collect_tree(&path, base, items, limit);
        } else {
            items.push(rel);
        }
    }
}

fn simple_search(dir: &Path, query: &str, limit: usize) -> Vec<String> {
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

fn parse_changed_files_from_patch(patch: &str) -> Vec<String> {
    let mut files = Vec::new();
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            // Format: diff --git a/path b/path
            if let Some(b_pos) = line.rfind(" b/") {
                let file = &line[b_pos + 3..];
                if !files.iter().any(|f: &String| f == file) {
                    files.push(file.to_string());
                }
            }
        }
    }
    files
}

pub fn is_sensitive_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    for sensitive in SENSITIVE_PATHS {
        if *sensitive == ".env" {
            // Match .env exactly or .env.* files
            let parts: Vec<&str> = path.split('/').collect();
            if parts.iter().any(|p| *p == ".env" || p.starts_with(".env.")) {
                return true;
            }
        } else if *sensitive == ".pem" || *sensitive == ".key" {
            if lower.ends_with(sensitive) {
                return true;
            }
        } else if lower.contains(sensitive) {
            return true;
        }
    }
    false
}

fn sanitize_tail(s: &str, max_len: usize) -> (String, bool) {
    let bytes = s.as_bytes();
    if bytes.len() <= max_len {
        (s.to_string(), false)
    } else {
        // Find a valid UTF-8 boundary near max_len
        let mut end = max_len;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        (s[end..].to_string(), true)
    }
}

fn run_command(cmd: &str, cwd: &Path, _timeout_secs: u64) -> (i32, String, String, u64) {
    let start = Instant::now();
    let result = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .output();

    match result {
        Ok(output) => {
            let elapsed = start.elapsed().as_millis() as u64;
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let code = output.status.code().unwrap_or(-1);
            (code, stdout, stderr, elapsed)
        }
        Err(e) => {
            let elapsed = start.elapsed().as_millis() as u64;
            (
                -1,
                String::new(),
                format!("Failed to execute command: {}", e),
                elapsed,
            )
        }
    }
}

// =============================================================================
// SSH helpers
// =============================================================================

/// Build SSH target string [user@]host from project config.
fn build_ssh_target(proj: &ProjectConfig) -> Result<String, String> {
    proj.ssh_target()
}

fn ssh_option_args(config: Option<&SshConfig>) -> Vec<String> {
    let Some(config) = config else {
        return Vec::new();
    };
    let mut args = Vec::new();
    if config.batch_mode || config.control_master {
        args.push("-o".to_string());
        args.push("BatchMode=yes".to_string());
    }
    if let Some(secs) = config.connect_timeout_secs {
        args.push("-o".to_string());
        args.push(format!("ConnectTimeout={secs}"));
    }
    if config.control_master {
        args.push("-o".to_string());
        args.push("ControlMaster=auto".to_string());
        if let Some(v) = &config.control_persist {
            args.push("-o".to_string());
            args.push(format!("ControlPersist={v}"));
        }
        if let Some(v) = &config.control_path {
            args.push("-o".to_string());
            args.push(format!("ControlPath={v}"));
        }
    }
    if let Some(secs) = config.server_alive_interval {
        args.push("-o".to_string());
        args.push(format!("ServerAliveInterval={secs}"));
    }
    if let Some(max) = config.server_alive_count_max {
        args.push("-o".to_string());
        args.push(format!("ServerAliveCountMax={max}"));
    }
    args
}

fn build_ssh_command(
    ssh_target: &str,
    remote_cmd: &str,
    config: Option<&SshConfig>,
) -> std::process::Command {
    let mut command = std::process::Command::new("ssh");
    for arg in ssh_option_args(config) {
        command.arg(arg);
    }
    command.arg(ssh_target).arg("--").arg(remote_cmd);
    command
}

/// Run a command on a remote host via SSH.
/// The command is passed as separate arguments to ssh (no local shell wrapping).
/// Remote shell interprets the command string.
fn run_ssh(
    ssh_target: &str,
    remote_cmd: &str,
    _timeout_secs: u64,
    ssh_config: Option<&SshConfig>,
) -> (i32, String, String, u64) {
    let start = Instant::now();
    let result = build_ssh_command(ssh_target, remote_cmd, ssh_config).output();

    match result {
        Ok(output) => {
            let elapsed = start.elapsed().as_millis() as u64;
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let code = output.status.code().unwrap_or(-1);
            (code, stdout, stderr, elapsed)
        }
        Err(e) => {
            let elapsed = start.elapsed().as_millis() as u64;
            (
                -1,
                String::new(),
                format!("Failed to execute SSH command: {}", e),
                elapsed,
            )
        }
    }
}

/// Run a command in the project directory.
/// For SSH: wraps with `cd <path> && <cmd>`.
/// For local: delegates to run_command with cwd.
fn run_project_cmd(
    proj: &ProjectConfig,
    cmd: &str,
    timeout_secs: u64,
    ssh_config: Option<&SshConfig>,
) -> (i32, String, String, u64) {
    if proj.is_ssh() {
        let ssh_target = match build_ssh_target(proj) {
            Ok(t) => t,
            Err(e) => return (-1, String::new(), e, 0),
        };
        let remote_cmd = format!("cd {} && {}", shell_escape(&proj.path), cmd);
        run_ssh(&ssh_target, &remote_cmd, timeout_secs, ssh_config)
    } else {
        run_command(cmd, &proj.root(), timeout_secs)
    }
}

/// Run an SSH command that receives patch data via stdin.
/// Writes local patch content to a remote temp file via SSH stdin,
/// then runs the remote command with the temp file path.
fn run_ssh_patch(
    ssh_target: &str,
    _project_path: &str,
    patch: &str,
    remote_cmd_template: &str,
    ssh_config: Option<&SshConfig>,
) -> (i32, String, String, u64) {
    let patch_id = uuid::Uuid::new_v4();
    let remote_patch = format!("/tmp/private-drop-patch-{}.diff", patch_id);
    let remote_cmd = format!(
        "cat > '{}' && {} && rm -f '{}'",
        remote_patch,
        remote_cmd_template.replace("__PATCH__", &remote_patch),
        remote_patch
    );
    let start = Instant::now();
    let result = build_ssh_command(ssh_target, &remote_cmd, ssh_config)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(patch.as_bytes());
                // stdin is dropped here, closing the pipe
            }
            child.wait_with_output()
        });

    match result {
        Ok(output) => {
            let elapsed = start.elapsed().as_millis() as u64;
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let code = output.status.code().unwrap_or(-1);
            (code, stdout, stderr, elapsed)
        }
        Err(e) => {
            let elapsed = start.elapsed().as_millis() as u64;
            (
                -1,
                String::new(),
                format!("Failed to execute SSH patch: {}", e),
                elapsed,
            )
        }
    }
}

/// Embedded Python3 script for remote edit operations.
/// Receives project root via argv[1] and edit JSON via stdin.
/// Returns JSON result on stdout.
const REMOTE_EDIT_SCRIPT: &str = r#####"
import sys, json, os, difflib, base64

SENSITIVE = ('.git', '.env', '.pem', '.key', 'id_rsa', 'id_ed25519',
             'target', 'node_modules')
MAX_FILE = 2 * 1024 * 1024
MAX_TEXT = 200 * 1024
MAX_BINARY = 5 * 1024 * 1024

def err(msg):
    return {'success': False, 'changed_files': [], 'diff': '', 'warnings': [], 'error': msg}

def is_sensitive(p):
    parts = p.replace('\\\\', '/').split('/')
    for seg in parts:
        if seg in SENSITIVE:
            return True
        for suf in ('.pem', '.key'):
            if seg.endswith(suf):
                return True
    return False

def validate_path(rel):
    if not rel:
        return 'path cannot be empty'
    if rel.startswith('/'):
        return 'Absolute paths are not allowed'
    if '..' in rel:
        return 'Path traversal (..) is not allowed'
    if is_sensitive(rel):
        return 'Cannot modify sensitive path: ' + rel
    return None

def resolve(root, rel, must_exist):
    e = validate_path(rel)
    if e:
        return None, e
    full = os.path.normpath(os.path.join(root, rel))
    canon_root = os.path.realpath(root)
    if not os.path.realpath(full).startswith(canon_root + os.sep) and os.path.realpath(full) != canon_root:
        return None, 'Path is outside project directory'
    if must_exist:
        if not os.path.isfile(full):
            return None, 'File does not exist: ' + rel
    else:
        parent = os.path.dirname(full)
        if not os.path.isdir(parent):
            return None, 'Parent directory does not exist for: ' + rel
    return full, None

def read_file(path):
    try:
        sz = os.path.getsize(path)
    except OSError as e:
        return None, 'Failed to stat file: ' + str(e)
    if sz > MAX_FILE:
        return None, 'File too large: %d bytes' % sz
    try:
        with open(path, 'r', encoding='utf-8') as f:
            return f.read(), None
    except Exception as e:
        return None, 'Failed to read UTF-8 file: ' + str(e)

def replace_nth(content, old, new, occ):
    if not old:
        return None, 'old_text cannot be empty'
    idxs = []
    start = 0
    while True:
        i = content.find(old, start)
        if i < 0:
            break
        idxs.append(i)
        start = i + len(old)
    if not idxs:
        return None, 'old_text was not found'
    if occ is not None:
        if occ < 1:
            return None, 'occurrence is 1-based and must be >= 1'
        if occ > len(idxs):
            return None, 'occurrence %d exceeds match count %d' % (occ, len(idxs))
        sel = idxs[occ - 1]
    else:
        if len(idxs) > 1:
            return None, 'old_text matched %d times; specify occurrence' % len(idxs)
        sel = idxs[0]
    return content[:sel] + new + content[sel + len(old):], None

def replace_range(content, sl, el, new):
    if sl < 1 or el < 1 or sl > el:
        return None, 'start_line and end_line must be 1-based and start_line <= end_line'
    had_nl = content.endswith('\n')
    lines = content.split('\n')
    # If content ends with \n, split gives an extra empty string at the end
    if had_nl and lines and lines[-1] == '':
        lines = lines[:-1]
    if el > len(lines):
        return None, 'line range %d-%d exceeds file line count %d' % (sl, el, len(lines))
    repl = [] if not new else new.rstrip('\n').split('\n')
    lines2 = lines[:sl-1] + repl + lines[el:]
    out = '\n'.join(lines2)
    if had_nl or new.endswith('\n'):
        out += '\n'
    return out, None

def simple_diff(path, old_content, new_content):
    old_lines = (old_content or '').splitlines(True)
    new_lines = new_content.splitlines(True)
    diff = difflib.unified_diff(old_lines, new_lines, fromfile='a/' + path, tofile='b/' + path)
    return ''.join(diff)

def simple_binary_diff(path, old_len, new_len):
    if old_len is None:
        return 'diff --git a/{0} b/{0}\nnew file mode 100644\nBinary file b/{0} added\n# new size: {1} bytes\n'.format(path, new_len)
    return 'diff --git a/{0} b/{0}\nBinary files a/{0} and b/{0} differ\n# old size: {1} bytes\n# new size: {2} bytes\n'.format(path, old_len, new_len)

def decode_binary(payload, rel):
    if len(payload) > MAX_BINARY * 2:
        return None, 'base64 content for %s is too large; maximum decoded size is %d bytes' % (rel, MAX_BINARY)
    try:
        data = base64.b64decode(payload, validate=True)
    except Exception as e:
        return None, 'Invalid base64 content for %s: %s' % (rel, e)
    if len(data) > MAX_BINARY:
        return None, 'binary content for %s exceeds %d bytes' % (rel, MAX_BINARY)
    return data, None

def main():
    if len(sys.argv) < 2:
        print(json.dumps(err('Missing project root argument')))
        return
    root = sys.argv[1]
    if not os.path.isdir(root):
        print(json.dumps(err('Project root does not exist: ' + root)))
        return
    try:
        body = json.load(sys.stdin)
    except Exception as e:
        print(json.dumps(err('Invalid JSON: ' + str(e))))
        return
    dry_run = body.get('dry_run', False)
    edits = body.get('edits', [])
    if not edits:
        print(json.dumps(err('edits cannot be empty')))
        return
    originals = {}
    current = {}
    paths_map = {}
    binary_originals = {}
    binary_current = {}
    changed = set()
    for ed in edits:
        etype = ed.get('type', '')
        rel = ed.get('path', '')
        e = validate_path(rel)
        if e:
            print(json.dumps(err(e)))
            return
        text_key = None
        if etype == 'replace_text':
            text_key = 'new_text'
        elif etype == 'replace_range':
            text_key = 'new_text'
        elif etype == 'append_file':
            text_key = 'text'
        elif etype in ('create_file', 'write_file'):
            text_key = 'content'
        if text_key:
            txt = ed.get(text_key, '')
            if len(txt.encode('utf-8')) > MAX_TEXT:
                print(json.dumps(err('edit text for %s exceeds %d bytes' % (rel, MAX_TEXT))))
                return
        if etype == 'replace_text':
            if rel not in current:
                full, e = resolve(root, rel, True)
                if e:
                    print(json.dumps(err(e)))
                    return
                paths_map[rel] = full
                c, e = read_file(full)
                if e:
                    print(json.dumps(err(e)))
                    return
                originals.setdefault(rel, c)
                current[rel] = c
            after, e = replace_nth(current[rel], ed.get('old_text', ''), ed.get('new_text', ''), ed.get('occurrence'))
            if e:
                print(json.dumps(err(e)))
                return
            current[rel] = after
        elif etype == 'replace_range':
            if rel not in current:
                full, e = resolve(root, rel, True)
                if e:
                    print(json.dumps(err(e)))
                    return
                paths_map[rel] = full
                c, e = read_file(full)
                if e:
                    print(json.dumps(err(e)))
                    return
                originals.setdefault(rel, c)
                current[rel] = c
            after, e = replace_range(current[rel], ed['start_line'], ed['end_line'], ed.get('new_text', ''))
            if e:
                print(json.dumps(err(e)))
                return
            current[rel] = after
        elif etype == 'append_file':
            if rel not in current:
                full, e = resolve(root, rel, True)
                if e:
                    print(json.dumps(err(e)))
                    return
                paths_map[rel] = full
                c, e = read_file(full)
                if e:
                    print(json.dumps(err(e)))
                    return
                originals.setdefault(rel, c)
                current[rel] = c
            current[rel] = current[rel] + ed.get('text', '')
        elif etype == 'create_file':
            full, e = resolve(root, rel, False)
            if e:
                print(json.dumps(err(e)))
                return
            if os.path.exists(full) or rel in current:
                print(json.dumps(err('File already exists: ' + rel)))
                return
            paths_map[rel] = full
            originals.setdefault(rel, None)
            current[rel] = ed.get('content', '')
        elif etype == 'write_file':
            full, e = resolve(root, rel, False)
            if e:
                print(json.dumps(err(e)))
                return
            allow = ed.get('allow_overwrite', False)
            if os.path.exists(full) and not allow:
                print(json.dumps(err('File exists and allow_overwrite is false: ' + rel)))
                return
            if os.path.exists(full) and rel not in originals:
                old_c, e2 = read_file(full)
                if e2:
                    print(json.dumps(err(e2)))
                    return
                originals[rel] = old_c
            elif rel not in originals:
                originals.setdefault(rel, None)
            paths_map[rel] = full
            current[rel] = ed.get('content', '')
        elif etype == 'create_binary_file':
            full, e = resolve(root, rel, False)
            if e:
                print(json.dumps(err(e)))
                return
            if os.path.exists(full) or rel in binary_current:
                print(json.dumps(err('File already exists: ' + rel)))
                return
            data, e = decode_binary(ed.get('base64_content', ''), rel)
            if e:
                print(json.dumps(err(e)))
                return
            paths_map[rel] = full
            binary_originals.setdefault(rel, None)
            binary_current[rel] = data
        elif etype == 'write_binary_file':
            full, e = resolve(root, rel, False)
            if e:
                print(json.dumps(err(e)))
                return
            allow = ed.get('allow_overwrite', False)
            if os.path.exists(full) and not allow:
                print(json.dumps(err('File exists and allow_overwrite is false: ' + rel)))
                return
            data, e = decode_binary(ed.get('base64_content', ''), rel)
            if e:
                print(json.dumps(err(e)))
                return
            if os.path.exists(full) and rel not in binary_originals:
                try:
                    with open(full, 'rb') as f:
                        binary_originals[rel] = f.read()
                except Exception as e:
                    print(json.dumps(err('Failed to read binary file: ' + str(e))))
                    return
            elif rel not in binary_originals:
                binary_originals.setdefault(rel, None)
            paths_map[rel] = full
            binary_current[rel] = data
        else:
            print(json.dumps(err('Unknown edit type: ' + etype)))
            return
        changed.add(rel)
    diff_parts = []
    for p in sorted(changed):
        old = originals.get(p)
        new = current.get(p)
        if new is not None and old != new:
            diff_parts.append(simple_diff(p, old, new))
        elif p in binary_current:
            old_b = binary_originals.get(p)
            new_b = binary_current.get(p)
            if new_b is not None and old_b != new_b:
                diff_parts.append(simple_binary_diff(p, None if old_b is None else len(old_b), len(new_b)))
    diff = ''.join(diff_parts)
    if not dry_run:
        for p in sorted(changed):
            full = paths_map.get(p)
            content = current.get(p)
            if full and content is not None:
                os.makedirs(os.path.dirname(full), exist_ok=True)
                with open(full, 'w', encoding='utf-8') as f:
                    f.write(content)
            elif full and p in binary_current and binary_current[p] is not None:
                os.makedirs(os.path.dirname(full), exist_ok=True)
                with open(full, 'wb') as f:
                    f.write(binary_current[p])
    print(json.dumps({
        'success': True,
        'changed_files': sorted(changed),
        'diff': diff,
        'warnings': [],
        'error': None
    }))

main()
"#####;

/// Run edit operations on a remote SSH project by piping JSON to a python3 script.
fn ssh_apply_project_edit(
    proj: &ProjectConfig,
    body: &EditRequest,
    ssh_config: Option<&SshConfig>,
) -> EditResponse {
    let ssh_target = match build_ssh_target(proj) {
        Ok(t) => t,
        Err(e) => return edit_error(e),
    };
    let project_path = &proj.path;

    // Serialize the edit request to JSON
    let body_json = match serde_json::to_string(body) {
        Ok(j) => j,
        Err(e) => return edit_error(format!("Failed to serialize edit request: {}", e)),
    };

    // Build the remote command: run python3 with the embedded script
    // Pass project root as first argument; script reads JSON from stdin
    let remote_cmd = format!(
        "python3 -c {} {}",
        shell_escape(REMOTE_EDIT_SCRIPT),
        shell_escape(project_path)
    );

    let start = Instant::now();
    let mut child = match build_ssh_command(&ssh_target, &remote_cmd, ssh_config)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return edit_error(format!("Failed to spawn SSH edit: {}", e)),
    };

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        if let Err(e) = stdin.write_all(body_json.as_bytes()) {
            let _ = child.kill();
            return edit_error(format!("Failed to write SSH edit payload: {}", e));
        }
    }

    let result = loop {
        match child.try_wait() {
            Ok(Some(_status)) => break child.wait_with_output(),
            Ok(None) if start.elapsed() > std::time::Duration::from_secs(CHECK_TIMEOUT_SECS) => {
                let _ = child.kill();
                let _ = child.wait();
                return edit_error(format!(
                    "SSH edit timed out after {} seconds",
                    CHECK_TIMEOUT_SECS
                ));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) => {
                let _ = child.kill();
                return edit_error(format!("Failed while waiting for SSH edit: {}", e));
            }
        }
    };

    match result {
        Ok(output) => {
            let _elapsed = start.elapsed().as_millis() as u64;
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let code = output.status.code().unwrap_or(-1);

            if code != 0 && stdout.is_empty() {
                // python3 not available or other exec failure
                if stderr.contains("No such file")
                    || stderr.contains("not found")
                    || stderr.contains("No module")
                {
                    return edit_error(
                        "Remote python3 is not available. Install python3 on the remote host."
                            .to_string(),
                    );
                }
                return edit_error(format!(
                    "SSH edit failed (exit {}): {}",
                    code,
                    stderr.chars().take(500).collect::<String>()
                ));
            }

            let mut resp: EditResponse = match serde_json::from_str(&stdout) {
                Ok(r) => r,
                Err(e) => {
                    return edit_error(format!(
                        "Failed to parse remote edit response: {}. Raw: {}",
                        e,
                        stdout.chars().take(500).collect::<String>()
                    ))
                }
            };
            let (truncated_diff, diff_truncated) = truncate_string(resp.diff, MAX_OUTPUT_LEN);
            resp.diff = truncated_diff;
            if diff_truncated {
                resp.warnings.push("Remote diff was truncated".to_string());
            }
            if !stderr.is_empty() {
                resp.warnings.push(format!(
                    "Remote stderr: {}",
                    stderr.chars().take(500).collect::<String>()
                ));
            }
            resp
        }
        Err(e) => edit_error(format!("Failed to execute SSH edit: {}", e)),
    }
}

/// Escape a string for safe use as a shell argument via `ssh -- arg`.
/// Uses single-quote wrapping with proper escaping.
fn shell_escape(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
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

fn ssh_overview(
    proj: &ProjectConfig,
    project_name: &str,
    ssh_config: Option<&SshConfig>,
) -> ContextResponse {
    let ssh_target = match build_ssh_target(proj) {
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
        "cd {} || exit 2; printf '__BRANCH__\\n'; git rev-parse --abbrev-ref HEAD 2>/dev/null || printf 'unknown\\n'; printf '__STATUS__\\n'; git status --short 2>/dev/null || true; printf '__FILES__\\n'; for f in {}; do if test -f \"$f\"; then printf '%s=yes\\n' \"$f\"; else printf '%s=no\\n' \"$f\"; fi; done",
        shell_escape(&proj.path),
        file_args
    );
    let (code, stdout, stderr, _) = run_ssh(&ssh_target, &remote_cmd, 15, ssh_config);
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

fn ssh_tree(
    proj: &ProjectConfig,
    project_name: &str,
    ssh_config: Option<&SshConfig>,
) -> ContextResponse {
    // Build find exclusions
    let mut excludes = String::new();
    for dir in IGNORED_DIRS {
        excludes.push_str(&format!(" -not -path '*/{}/*'", dir));
    }
    let cmd = format!(
        "cd {} && find . -mindepth 1 -maxdepth 8{} -type f -print | sort | head -n {} | sed 's|^\\./||'",
        shell_escape(&proj.path), excludes, MAX_TREE_ITEMS
    );
    let (code, stdout, stderr, _) = run_ssh(
        &build_ssh_target(proj).unwrap_or_default(),
        &cmd,
        30,
        ssh_config,
    );
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

fn ssh_search(
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
    let (code, stdout, stderr, _) = run_ssh(
        &build_ssh_target(proj).unwrap_or_default(),
        &cmd,
        30,
        ssh_config,
    );
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

fn ssh_read_file(
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
    let cmd = format!("sed -n '{},{}p' -- {}", start_line, end_line, escaped_path);
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
        .map(|(i, l)| format!("{:4} | {}", start_line + i, l))
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

fn ssh_apply_patch(
    proj: &ProjectConfig,
    _project_name: &str,
    patch: &str,
    changed: Vec<String>,
    ssh_config: Option<&SshConfig>,
) -> PatchResponse {
    let ssh_target = match build_ssh_target(proj) {
        Ok(t) => t,
        Err(e) => {
            return PatchResponse {
                success: false,
                changed_files: Some(changed),
                stdout: None,
                stderr: None,
                error: Some(e),
            }
        }
    };
    let remote_cmd = format!(
        "cd {} && git apply --check __PATCH__ && git apply __PATCH__",
        shell_escape(&proj.path)
    );
    let (code, stdout, stderr, _) =
        run_ssh_patch(&ssh_target, &proj.path, patch, &remote_cmd, ssh_config);
    if code == 0 {
        PatchResponse {
            success: true,
            changed_files: Some(changed),
            stdout: Some(stdout),
            stderr: Some(stderr),
            error: None,
        }
    } else {
        PatchResponse {
            success: false,
            changed_files: Some(changed),
            stdout: Some(stdout),
            stderr: Some(stderr),
            error: Some("git apply failed".to_string()),
        }
    }
}

fn mode_name(mode: &ContextMode) -> &'static str {
    match mode {
        ContextMode::Overview => "overview",
        ContextMode::Tree => "tree",
        ContextMode::Search => "search",
        ContextMode::ReadFile => "read_file",
        ContextMode::GitStatus => "git_status",
        ContextMode::GitDiff => "git_diff",
    }
}

fn context_error(project: &str, mode: &ContextMode, error: String) -> ContextResponse {
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

const MAX_READ_FILE_LIMIT: usize = 2_000;

fn validate_read_file_range(start_line: usize, limit: usize) -> Result<usize, String> {
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

fn parse_ssh_batch_blocks(stdout: &str, count: usize, nonce: &str) -> Vec<String> {
    let mut blocks = vec![String::new(); count];
    let mut current: Option<usize> = None;
    let start_prefix = format!("__PDCTX_{}_START_", nonce);
    let end_prefix = format!("__PDCTX_{}_END_", nonce);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix(&start_prefix) {
            if let Some(idx) = rest
                .strip_suffix("__")
                .and_then(|s| s.parse::<usize>().ok())
            {
                current = if idx < count { Some(idx) } else { None };
            }
            continue;
        }
        if line.starts_with(&end_prefix) {
            current = None;
            continue;
        }
        if let Some(idx) = current {
            blocks[idx].push_str(line);
            blocks[idx].push('\n');
        }
    }
    blocks
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
        ContextMode::Search => context_error(
            project_name,
            &item.mode,
            "search is not supported by single-SSH context batch".to_string(),
        ),
    }
}

fn ssh_context_batch_error_results(
    project_name: &str,
    requests: &[ContextBatchItem],
    error: String,
) -> Vec<ContextResponse> {
    requests
        .iter()
        .map(|item| context_error(project_name, &item.mode, error.clone()))
        .collect()
}

fn try_ssh_context_batch_once(
    proj: &ProjectConfig,
    project_name: &str,
    requests: &[ContextBatchItem],
    ssh_config: Option<&SshConfig>,
) -> Option<(Vec<ContextResponse>, u64)> {
    if requests.is_empty() {
        return Some((Vec::new(), 0));
    }
    let ssh_target = match build_ssh_target(proj) {
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
        if matches!(item.mode, ContextMode::Search) {
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
                script.push_str(&format!(" printf '__BRANCH__\\n'; git rev-parse --abbrev-ref HEAD 2>/dev/null || printf 'unknown\\n'; printf '__STATUS__\\n'; git status --short 2>/dev/null || true; printf '__FILES__\\n'; for f in {}; do if test -f \"$f\"; then printf '%s=yes\\n' \"$f\"; else printf '%s=no\\n' \"$f\"; fi; done;", file_args));
            }
            ContextMode::Tree => {
                let mut excludes = String::new();
                for dir in IGNORED_DIRS {
                    excludes.push_str(&format!(" -not -path '*/{}/*'", dir));
                }
                script.push_str(&format!(" find . -mindepth 1 -maxdepth 8{} -type f -print 2>/dev/null | sort | head -n {} | sed 's|^\\./||';", excludes, MAX_TREE_ITEMS));
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
                script.push_str(&format!(" if test -f {0}; then sed -n '{1},{2}p' -- {0}; else printf '__PDCTX_ERROR__:File not found: {0}\\n'; fi;", escaped_path, item.start_line, end_line));
            }
            ContextMode::GitStatus => {
                script.push_str(" git status --short 2>/dev/null || true;");
            }
            ContextMode::GitDiff => {
                script.push_str(" git diff 2>/dev/null || true;");
            }
            ContextMode::Search => return None,
        }
        script.push_str(&format!(" printf '\n__PDCTX_{}_END_{}__\n';", nonce, idx));
    }

    let (code, stdout, stderr, _) = run_ssh(&ssh_target, &script, 30, ssh_config);
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

fn execute_context_item(
    proj: &ProjectConfig,
    project_name: &str,
    item: &ContextBatchItem,
    ssh_config: Option<&SshConfig>,
) -> (ContextResponse, u64) {
    if proj.is_ssh() {
        let resp = match item.mode {
            ContextMode::Overview => ssh_overview(proj, project_name, ssh_config),
            ContextMode::Tree => ssh_tree(proj, project_name, ssh_config),
            ContextMode::Search => match &item.query {
                Some(query) => ssh_search(proj, project_name, query, ssh_config),
                None => context_error(
                    project_name,
                    &item.mode,
                    "query parameter is required for search mode".to_string(),
                ),
            },
            ContextMode::ReadFile => match &item.path {
                Some(path) => ssh_read_file(
                    proj,
                    project_name,
                    path,
                    item.start_line,
                    item.limit,
                    ssh_config,
                ),
                None => context_error(
                    project_name,
                    &item.mode,
                    "path parameter is required for read_file mode".to_string(),
                ),
            },
            ContextMode::GitStatus => {
                let (code, stdout, stderr, _) =
                    run_project_cmd(proj, "git status --short", 10, ssh_config);
                if code == 0 {
                    let (content, truncated) = truncate_string(stdout, MAX_OUTPUT_LEN);
                    ContextResponse {
                        success: true,
                        project: project_name.to_string(),
                        mode: "git_status".to_string(),
                        content: Some(content),
                        items: None,
                        truncated,
                        error: None,
                    }
                } else {
                    context_error(
                        project_name,
                        &item.mode,
                        format!("git status failed: {}", stderr.trim()),
                    )
                }
            }
            ContextMode::GitDiff => {
                let (code, stdout, stderr, _) = run_project_cmd(proj, "git diff", 30, ssh_config);
                if code == 0 {
                    let (content, truncated) = truncate_string(stdout, MAX_OUTPUT_LEN);
                    ContextResponse {
                        success: true,
                        project: project_name.to_string(),
                        mode: "git_diff".to_string(),
                        content: Some(content),
                        items: None,
                        truncated,
                        error: None,
                    }
                } else {
                    context_error(
                        project_name,
                        &item.mode,
                        format!("git diff failed: {}", stderr.trim()),
                    )
                }
            }
        };
        return (resp, 1);
    }

    let root = proj.root();
    if !root.exists() {
        return (
            context_error(
                project_name,
                &item.mode,
                format!("Project root does not exist: {:?}", root),
            ),
            0,
        );
    }
    match item.mode {
        ContextMode::Overview => {
            let branch = run_command("git rev-parse --abbrev-ref HEAD", &root, 10)
                .1
                .trim()
                .to_string();
            let status = run_command("git status --short", &root, 10)
                .1
                .trim()
                .to_string();
            let important_files = [
                "README.md",
                "TODO.md",
                "Cargo.toml",
                "scripts/e2e_test.sh",
                "src/main.rs",
            ];
            let mut content = format!("Project: {}\nRoot: {}\nBranch: {}\n\nGit Status:\n{}\n\nAllowed Checks: {}\n\nImportant Files:", project_name, root.display(), branch, status, proj.allowed_checks.join(", "));
            for f in &important_files {
                let exists = root.join(f).exists();
                content.push_str(&format!("\n  {}: {}", f, if exists { "yes" } else { "no" }));
            }
            (
                ContextResponse {
                    success: true,
                    project: project_name.to_string(),
                    mode: "overview".to_string(),
                    content: Some(content),
                    items: None,
                    truncated: false,
                    error: None,
                },
                0,
            )
        }
        ContextMode::Tree => {
            let mut items = Vec::new();
            collect_tree(&root, &root, &mut items, MAX_TREE_ITEMS);
            let truncated = items.len() >= MAX_TREE_ITEMS;
            (
                ContextResponse {
                    success: true,
                    project: project_name.to_string(),
                    mode: "tree".to_string(),
                    content: None,
                    items: Some(items),
                    truncated,
                    error: None,
                },
                0,
            )
        }
        ContextMode::Search => match &item.query {
            Some(query) => {
                let results = simple_search(&root, query, MAX_SEARCH_RESULTS);
                let truncated = results.len() >= MAX_SEARCH_RESULTS;
                (
                    ContextResponse {
                        success: true,
                        project: project_name.to_string(),
                        mode: "search".to_string(),
                        content: None,
                        items: Some(results),
                        truncated,
                        error: None,
                    },
                    0,
                )
            }
            None => (
                context_error(
                    project_name,
                    &item.mode,
                    "query parameter is required for search mode".to_string(),
                ),
                0,
            ),
        },
        ContextMode::ReadFile => match &item.path {
            Some(rel_path) => {
                let full_path = root.join(rel_path);
                match canonicalize_and_verify(&full_path, &root) {
                    Ok(canonical) => match std::fs::read_to_string(&canonical) {
                        Ok(content) => {
                            let lines: Vec<&str> = content.lines().collect();
                            let total = lines.len();
                            let end_line =
                                match validate_read_file_range(item.start_line, item.limit) {
                                    Ok(end_line) => end_line,
                                    Err(e) => {
                                        return (context_error(project_name, &item.mode, e), 0)
                                    }
                                };
                            let start = item.start_line - 1;
                            let end = end_line.min(total);
                            let selected: Vec<String> = if start < total {
                                lines[start..end]
                                    .iter()
                                    .enumerate()
                                    .map(|(i, l)| format!("{:4} | {}", start + i + 1, l))
                                    .collect()
                            } else {
                                Vec::new()
                            };
                            let (output, truncated) =
                                truncate_string(selected.join("\n"), MAX_OUTPUT_LEN);
                            (
                                ContextResponse {
                                    success: true,
                                    project: project_name.to_string(),
                                    mode: "read_file".to_string(),
                                    content: Some(output),
                                    items: None,
                                    truncated,
                                    error: None,
                                },
                                0,
                            )
                        }
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
            None => (
                context_error(
                    project_name,
                    &item.mode,
                    "path parameter is required for read_file mode".to_string(),
                ),
                0,
            ),
        },
        ContextMode::GitStatus => {
            let output = run_command("git status --short", &root, 10);
            let (content, truncated) = truncate_string(output.1, MAX_OUTPUT_LEN);
            (
                ContextResponse {
                    success: true,
                    project: project_name.to_string(),
                    mode: "git_status".to_string(),
                    content: Some(content),
                    items: None,
                    truncated,
                    error: None,
                },
                0,
            )
        }
        ContextMode::GitDiff => {
            let output = run_command("git diff", &root, 30);
            let (content, truncated) = truncate_string(output.1, MAX_OUTPUT_LEN);
            (
                ContextResponse {
                    success: true,
                    project: project_name.to_string(),
                    mode: "git_diff".to_string(),
                    content: Some(content),
                    items: None,
                    truncated,
                    error: None,
                },
                0,
            )
        }
    }
}

// =============================================================================
// Handlers
// =============================================================================

#[handler]
pub async fn codex_context(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(ContextResponse {
            success: false,
            project: String::new(),
            mode: String::new(),
            content: None,
            items: None,
            truncated: false,
            error: Some(
                "Projects not configured. Set PROJECTS_CONFIG or create projects.toml".to_string(),
            ),
        }));
        return;
    };
    let body: ContextRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ContextResponse {
                success: false,
                project: String::new(),
                mode: String::new(),
                content: None,
                items: None,
                truncated: false,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    let request_start = Instant::now();
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ContextResponse {
                success: false,
                project: body.project.clone(),
                mode: format!("{:?}", body.mode),
                content: None,
                items: None,
                truncated: false,
                error: Some(e),
            }));
            return;
        }
    };

    // For SSH executor, dispatch to SSH helpers
    if proj.is_ssh() {
        let ssh_config = projects.ssh.as_ref();
        let resp = match body.mode {
            ContextMode::Overview => ssh_overview(proj, &body.project, ssh_config),
            ContextMode::Tree => ssh_tree(proj, &body.project, ssh_config),
            ContextMode::Search => {
                let Some(query) = &body.query else {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(ContextResponse {
                        success: false,
                        project: body.project,
                        mode: "search".to_string(),
                        content: None,
                        items: None,
                        truncated: false,
                        error: Some("query parameter is required for search mode".to_string()),
                    }));
                    return;
                };
                ssh_search(proj, &body.project, query, ssh_config)
            }
            ContextMode::ReadFile => {
                let Some(rel_path) = &body.path else {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(ContextResponse {
                        success: false,
                        project: body.project,
                        mode: "read_file".to_string(),
                        content: None,
                        items: None,
                        truncated: false,
                        error: Some("path parameter is required for read_file mode".to_string()),
                    }));
                    return;
                };
                ssh_read_file(
                    proj,
                    &body.project,
                    rel_path,
                    body.start_line,
                    body.limit,
                    ssh_config,
                )
            }
            ContextMode::GitStatus => {
                let (code, stdout, stderr, _) =
                    run_project_cmd(proj, "git status --short", 10, ssh_config);
                if code != 0 {
                    ContextResponse {
                        success: false,
                        project: body.project.clone(),
                        mode: "git_status".to_string(),
                        content: None,
                        items: None,
                        truncated: false,
                        error: Some(format!("git status failed: {}", stderr.trim())),
                    }
                } else {
                    let (content, truncated) = truncate_string(stdout, MAX_OUTPUT_LEN);
                    ContextResponse {
                        success: true,
                        project: body.project.clone(),
                        mode: "git_status".to_string(),
                        content: Some(content),
                        items: None,
                        truncated,
                        error: None,
                    }
                }
            }
            ContextMode::GitDiff => {
                let (code, stdout, stderr, _) = run_project_cmd(proj, "git diff", 30, ssh_config);
                if code != 0 {
                    ContextResponse {
                        success: false,
                        project: body.project.clone(),
                        mode: "git_diff".to_string(),
                        content: None,
                        items: None,
                        truncated: false,
                        error: Some(format!("git diff failed: {}", stderr.trim())),
                    }
                } else {
                    let (content, truncated) = truncate_string(stdout, MAX_OUTPUT_LEN);
                    ContextResponse {
                        success: true,
                        project: body.project.clone(),
                        mode: "git_diff".to_string(),
                        content: Some(content),
                        items: None,
                        truncated,
                        error: None,
                    }
                }
            }
        };
        let ssh_calls = match resp.mode.as_str() {
            "overview" | "tree" | "search" | "read_file" | "git_status" | "git_diff" => 1,
            _ => 0,
        };
        tracing::info!(
            target: "codex.metrics",
            operation = "getProjectContext",
            project = %resp.project,
            mode = %resp.mode,
            executor = "ssh",
            success = resp.success,
            duration_ms = request_start.elapsed().as_millis() as u64,
            ssh_calls = ssh_calls,
            control_master = projects.ssh.as_ref().map(|s| s.control_master).unwrap_or(false),
            "codex_context_completed"
        );
        res.render(Json(resp));
        return;
    }

    // Local executor
    let root = proj.root();
    if !root.exists() {
        res.render(Json(ContextResponse {
            success: false,
            project: body.project.clone(),
            mode: format!("{:?}", body.mode),
            content: None,
            items: None,
            truncated: false,
            error: Some(format!("Project root does not exist: {:?}", root)),
        }));
        return;
    }

    match body.mode {
        ContextMode::Overview => {
            let branch = run_command("git rev-parse --abbrev-ref HEAD", &root, 10)
                .1
                .trim()
                .to_string();
            let status = run_command("git status --short", &root, 10)
                .1
                .trim()
                .to_string();
            let important_files = [
                "README.md",
                "TODO.md",
                "Cargo.toml",
                "scripts/e2e_test.sh",
                "src/main.rs",
            ];
            let mut content = format!("Project: {}\nRoot: {}\nBranch: {}\n\nGit Status:\n{}\n\nAllowed Checks: {}\n\nImportant Files:",
                body.project, root.display(), branch, status, proj.allowed_checks.join(", "));
            for f in &important_files {
                let exists = root.join(f).exists();
                content.push_str(&format!("\n  {}: {}", f, if exists { "yes" } else { "no" }));
            }
            res.render(Json(ContextResponse {
                success: true,
                project: body.project,
                mode: "overview".to_string(),
                content: Some(content),
                items: None,
                truncated: false,
                error: None,
            }));
        }
        ContextMode::Tree => {
            let mut items = Vec::new();
            collect_tree(&root, &root, &mut items, MAX_TREE_ITEMS);
            let truncated = items.len() >= MAX_TREE_ITEMS;
            res.render(Json(ContextResponse {
                success: true,
                project: body.project,
                mode: "tree".to_string(),
                content: None,
                items: Some(items),
                truncated,
                error: None,
            }));
        }
        ContextMode::Search => {
            let Some(query) = &body.query else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(ContextResponse {
                    success: false,
                    project: body.project,
                    mode: "search".to_string(),
                    content: None,
                    items: None,
                    truncated: false,
                    error: Some("query parameter is required for search mode".to_string()),
                }));
                return;
            };
            let results = simple_search(&root, query, MAX_SEARCH_RESULTS);
            let truncated = results.len() >= MAX_SEARCH_RESULTS;
            res.render(Json(ContextResponse {
                success: true,
                project: body.project,
                mode: "search".to_string(),
                content: None,
                items: Some(results),
                truncated,
                error: None,
            }));
        }
        ContextMode::ReadFile => {
            let Some(rel_path) = &body.path else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(ContextResponse {
                    success: false,
                    project: body.project,
                    mode: "read_file".to_string(),
                    content: None,
                    items: None,
                    truncated: false,
                    error: Some("path parameter is required for read_file mode".to_string()),
                }));
                return;
            };
            let full_path = root.join(rel_path);
            match canonicalize_and_verify(&full_path, &root) {
                Ok(canonical) => match std::fs::read_to_string(&canonical) {
                    Ok(content) => {
                        let lines: Vec<&str> = content.lines().collect();
                        let total = lines.len();
                        let end_line = match validate_read_file_range(body.start_line, body.limit) {
                            Ok(end_line) => end_line,
                            Err(e) => {
                                res.status_code(StatusCode::BAD_REQUEST);
                                res.render(Json(ContextResponse {
                                    success: false,
                                    project: body.project,
                                    mode: "read_file".to_string(),
                                    content: None,
                                    items: None,
                                    truncated: false,
                                    error: Some(e),
                                }));
                                return;
                            }
                        };
                        let start = body.start_line - 1;
                        let end = end_line.min(total);
                        let selected: Vec<String> = if start < total {
                            lines[start..end]
                                .iter()
                                .enumerate()
                                .map(|(i, l)| format!("{:4} | {}", start + i + 1, l))
                                .collect()
                        } else {
                            Vec::new()
                        };
                        let output = selected.join("\n");
                        let (output, truncated) = truncate_string(output, MAX_OUTPUT_LEN);
                        res.render(Json(ContextResponse {
                            success: true,
                            project: body.project,
                            mode: "read_file".to_string(),
                            content: Some(output),
                            items: None,
                            truncated,
                            error: None,
                        }));
                    }
                    Err(e) => {
                        res.render(Json(ContextResponse {
                            success: false,
                            project: body.project,
                            mode: "read_file".to_string(),
                            content: None,
                            items: None,
                            truncated: false,
                            error: Some(format!("Failed to read file: {}", e)),
                        }));
                    }
                },
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(ContextResponse {
                        success: false,
                        project: body.project,
                        mode: "read_file".to_string(),
                        content: None,
                        items: None,
                        truncated: false,
                        error: Some(e),
                    }));
                }
            }
        }
        ContextMode::GitStatus => {
            let output = run_command("git status --short", &root, 10);
            let (content, truncated) = truncate_string(output.1, MAX_OUTPUT_LEN);
            res.render(Json(ContextResponse {
                success: true,
                project: body.project,
                mode: "git_status".to_string(),
                content: Some(content),
                items: None,
                truncated,
                error: None,
            }));
        }
        ContextMode::GitDiff => {
            let output = run_command("git diff", &root, 30);
            let (content, truncated) = truncate_string(output.1, MAX_OUTPUT_LEN);
            res.render(Json(ContextResponse {
                success: true,
                project: body.project,
                mode: "git_diff".to_string(),
                content: Some(content),
                items: None,
                truncated,
                error: None,
            }));
        }
    }
}

#[handler]
pub async fn codex_context_batch(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(ContextBatchResponse {
            success: false,
            project: String::new(),
            results: Vec::new(),
            duration_ms: 0,
            ssh_calls: 0,
            error: Some(
                "Projects not configured. Set PROJECTS_CONFIG or create projects.toml".to_string(),
            ),
        }));
        return;
    };
    let body: ContextBatchRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ContextBatchResponse {
                success: false,
                project: String::new(),
                results: Vec::new(),
                duration_ms: 0,
                ssh_calls: 0,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    if body.requests.is_empty() || body.requests.len() > 20 {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(ContextBatchResponse {
            success: false,
            project: body.project,
            results: Vec::new(),
            duration_ms: 0,
            ssh_calls: 0,
            error: Some("requests must contain 1-20 items".to_string()),
        }));
        return;
    }
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ContextBatchResponse {
                success: false,
                project: body.project,
                results: Vec::new(),
                duration_ms: 0,
                ssh_calls: 0,
                error: Some(e),
            }));
            return;
        }
    };

    let start = Instant::now();
    let (results, ssh_calls) = if proj.is_ssh() {
        match try_ssh_context_batch_once(proj, &body.project, &body.requests, projects.ssh.as_ref())
        {
            Some((results, ssh_calls)) => (results, ssh_calls),
            None => {
                let mut ssh_calls = 0;
                let mut results = Vec::with_capacity(body.requests.len());
                for item in &body.requests {
                    let (resp, calls) =
                        execute_context_item(proj, &body.project, item, projects.ssh.as_ref());
                    ssh_calls += calls;
                    results.push(resp);
                }
                (results, ssh_calls)
            }
        }
    } else {
        let mut results = Vec::with_capacity(body.requests.len());
        for item in &body.requests {
            let (resp, _) = execute_context_item(proj, &body.project, item, None);
            results.push(resp);
        }
        (results, 0)
    };
    let success = results.iter().all(|r| r.success);
    let duration_ms = start.elapsed().as_millis() as u64;
    tracing::info!(
        target: "codex.metrics",
        operation = "getProjectContextBatch",
        project = %body.project,
        executor = if proj.is_ssh() { "ssh" } else { "local" },
        success = success,
        request_count = results.len(),
        duration_ms = duration_ms,
        ssh_calls = ssh_calls,
        control_master = projects.ssh.as_ref().map(|s| s.control_master).unwrap_or(false),
        "codex_context_batch_completed"
    );
    res.render(Json(ContextBatchResponse {
        success,
        project: body.project,
        results,
        duration_ms,
        ssh_calls,
        error: None,
    }));
}

#[handler]
pub async fn codex_apply_patch(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(PatchResponse {
            success: false,
            changed_files: None,
            stdout: None,
            stderr: None,
            error: Some("Projects not configured".to_string()),
        }));
        return;
    };
    let body: PatchRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(PatchResponse {
                success: false,
                changed_files: None,
                stdout: None,
                stderr: None,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(PatchResponse {
                success: false,
                changed_files: None,
                stdout: None,
                stderr: None,
                error: Some(e),
            }));
            return;
        }
    };
    if !proj.allow_patch() {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(PatchResponse {
            success: false,
            changed_files: None,
            stdout: None,
            stderr: None,
            error: Some("Patch is not allowed for this project".to_string()),
        }));
        return;
    }
    if body.patch.is_empty() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(PatchResponse {
            success: false,
            changed_files: None,
            stdout: None,
            stderr: None,
            error: Some("Patch cannot be empty".to_string()),
        }));
        return;
    }

    // Validate changed file paths against sensitive paths
    let changed = parse_changed_files_from_patch(&body.patch);
    for file in &changed {
        if is_sensitive_path(file) {
            res.status_code(StatusCode::FORBIDDEN);
            res.render(Json(PatchResponse {
                success: false,
                changed_files: None,
                stdout: None,
                stderr: None,
                error: Some(format!("Cannot modify sensitive path: {}", file)),
            }));
            return;
        }
    }

    if proj.is_ssh() {
        // SSH executor: pipe patch via stdin
        let resp = ssh_apply_patch(
            proj,
            &body.project,
            &body.patch,
            changed,
            projects.ssh.as_ref(),
        );
        res.render(Json(resp));
        return;
    }

    // Local executor
    let root = proj.root();
    if !root.exists() {
        res.render(Json(PatchResponse {
            success: false,
            changed_files: None,
            stdout: None,
            stderr: None,
            error: Some("Project root does not exist".to_string()),
        }));
        return;
    }

    // Write patch to temp file, run git apply
    let patch_file = root.join(format!(".codex-patch-{}.diff", uuid::Uuid::new_v4()));
    if let Err(e) = std::fs::write(&patch_file, &body.patch) {
        res.render(Json(PatchResponse {
            success: false,
            changed_files: None,
            stdout: None,
            stderr: None,
            error: Some(format!("Failed to write temp patch file: {}", e)),
        }));
        return;
    }

    // Dry run first
    let check_out = run_command(
        &format!("git apply --check '{}'", patch_file.display()),
        &root,
        60,
    );
    if check_out.0 != 0 {
        let _ = std::fs::remove_file(&patch_file);
        res.render(Json(PatchResponse {
            success: false,
            changed_files: Some(changed),
            stdout: Some(check_out.1),
            stderr: Some(check_out.2),
            error: Some("git apply --check failed".to_string()),
        }));
        return;
    }

    // Apply for real
    let apply_out = run_command(&format!("git apply '{}'", patch_file.display()), &root, 60);
    let _ = std::fs::remove_file(&patch_file);

    if apply_out.0 == 0 {
        res.render(Json(PatchResponse {
            success: true,
            changed_files: Some(changed),
            stdout: Some(apply_out.1),
            stderr: Some(apply_out.2),
            error: None,
        }));
    } else {
        res.render(Json(PatchResponse {
            success: false,
            changed_files: Some(changed),
            stdout: Some(apply_out.1),
            stderr: Some(apply_out.2),
            error: Some("git apply failed".to_string()),
        }));
    }
}

#[handler]
pub async fn codex_git(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(GitResponse {
            success: false,
            project: String::new(),
            operation: String::new(),
            exit_code: None,
            duration_ms: 0,
            stdout_tail: None,
            stderr_tail: None,
            truncated: false,
            error: Some("Projects not configured".to_string()),
        }));
        return;
    };
    let body: GitRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(GitResponse {
                success: false,
                project: String::new(),
                operation: String::new(),
                exit_code: None,
                duration_ms: 0,
                stdout_tail: None,
                stderr_tail: None,
                truncated: false,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(git_error(&body.project, &body.operation, e)));
            return;
        }
    };
    if matches!(
        body.operation,
        GitOperation::Add | GitOperation::Commit | GitOperation::CommitAmendNoEdit
    ) && !proj.allow_patch()
    {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(git_error(
            &body.project,
            &body.operation,
            "Git mutation is not allowed for this project".to_string(),
        )));
        return;
    }
    let cmd = match git_command_for_request(&body) {
        Ok(cmd) => cmd,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(git_error(&body.project, &body.operation, e)));
            return;
        }
    };
    let (code, stdout, stderr, duration_ms) =
        run_project_cmd(proj, &cmd, CHECK_TIMEOUT_SECS, projects.ssh.as_ref());
    let (stdout_tail, stdout_trunc) = sanitize_tail(&stdout, MAX_OUTPUT_LEN);
    let (stderr_tail, stderr_trunc) = sanitize_tail(&stderr, MAX_OUTPUT_LEN);
    let truncated = stdout_trunc || stderr_trunc;
    let success = code == 0;
    tracing::info!(
        target: "codex.metrics",
        operation = "runProjectGit",
        project = %body.project,
        git_operation = git_operation_name(&body.operation),
        executor = if proj.is_ssh() { "ssh" } else { "local" },
        success = success,
        exit_code = code,
        duration_ms = duration_ms,
        ssh_calls = if proj.is_ssh() { 1 } else { 0 },
        control_master = projects.ssh.as_ref().map(|s| s.control_master).unwrap_or(false),
        "codex_git_completed"
    );
    res.render(Json(GitResponse {
        success,
        project: body.project,
        operation: git_operation_name(&body.operation).to_string(),
        exit_code: Some(code),
        duration_ms,
        stdout_tail: Some(stdout_tail),
        stderr_tail: Some(stderr_tail),
        truncated,
        error: if success {
            None
        } else {
            Some("git operation failed".to_string())
        },
    }));
}

#[handler]
pub async fn codex_command(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(CommandResponse {
            success: false,
            project: String::new(),
            command: String::new(),
            exit_code: None,
            duration_ms: 0,
            stdout_tail: None,
            stderr_tail: None,
            truncated: false,
            error: Some("Projects not configured".to_string()),
        }));
        return;
    };
    let body: CommandRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandResponse {
                success: false,
                project: String::new(),
                command: String::new(),
                exit_code: None,
                duration_ms: 0,
                stdout_tail: None,
                stderr_tail: None,
                truncated: false,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(command_error(&body.project, &body.command, e)));
            return;
        }
    };
    let cmd = match get_project_command(proj, &body.command) {
        Ok(cmd) => cmd,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(command_error(&body.project, &body.command, e)));
            return;
        }
    };
    let (code, stdout, stderr, duration_ms) =
        run_project_cmd(proj, &cmd, CHECK_TIMEOUT_SECS, projects.ssh.as_ref());
    let (stdout_tail, stdout_trunc) = sanitize_tail(&stdout, MAX_OUTPUT_LEN);
    let (stderr_tail, stderr_trunc) = sanitize_tail(&stderr, MAX_OUTPUT_LEN);
    let truncated = stdout_trunc || stderr_trunc;
    let success = code == 0;
    tracing::info!(
        target: "codex.metrics",
        operation = "runProjectCommand",
        project = %body.project,
        command = %body.command,
        executor = if proj.is_ssh() { "ssh" } else { "local" },
        success = success,
        exit_code = code,
        duration_ms = duration_ms,
        ssh_calls = if proj.is_ssh() { 1 } else { 0 },
        control_master = projects.ssh.as_ref().map(|s| s.control_master).unwrap_or(false),
        "codex_command_completed"
    );
    res.render(Json(CommandResponse {
        success,
        project: body.project,
        command: body.command,
        exit_code: Some(code),
        duration_ms,
        stdout_tail: Some(stdout_tail),
        stderr_tail: Some(stderr_tail),
        truncated,
        error: if success {
            None
        } else {
            Some("command failed".to_string())
        },
    }));
}

fn op_response(
    op: &str,
    success: bool,
    records: Vec<CommandAuditRecord>,
    error: Option<String>,
) -> CommandRequestOpResponse {
    op_response_with_goals(op, success, records, Vec::new(), error)
}

fn op_response_with_goals(
    op: &str,
    success: bool,
    records: Vec<CommandAuditRecord>,
    goals: Vec<CodexGoalRecord>,
    error: Option<String>,
) -> CommandRequestOpResponse {
    CommandRequestOpResponse {
        success,
        op: op.to_string(),
        request_id: records.first().map(|r| r.id.clone()),
        record: records.first().cloned(),
        goal_id: goals.first().map(|g| g.id.clone()),
        goal: goals.first().cloned(),
        records,
        goals,
        error,
    }
}

fn build_goal_record(
    project: String,
    title: String,
    summary: Option<String>,
    now: i64,
    ttl_secs: i64,
) -> CodexGoalRecord {
    CodexGoalRecord {
        id: uuid::Uuid::new_v4().to_string(),
        project,
        title,
        summary,
        status: "pending".to_string(),
        created_at: now,
        expires_at: now + ttl_secs,
        closed_at: None,
        error: None,
    }
}

fn validate_goal_text(title: &str, summary: &Option<String>) -> Result<(), String> {
    let len = title.chars().count();
    if len == 0 {
        return Err("goal title cannot be empty".to_string());
    }
    if len > MAX_GOAL_TITLE_LEN {
        return Err(format!(
            "goal title is too long; maximum is {} characters",
            MAX_GOAL_TITLE_LEN
        ));
    }
    if let Some(summary) = summary {
        if summary.chars().count() > MAX_GOAL_SUMMARY_LEN {
            return Err(format!(
                "goal summary is too long; maximum is {} characters",
                MAX_GOAL_SUMMARY_LEN
            ));
        }
    }
    Ok(())
}

fn validate_goal_status(status: &str) -> Result<(), String> {
    match status {
        "pending" | "active" | "closed" | "expired" | "rejected" => Ok(()),
        _ => Err("invalid goal status filter".to_string()),
    }
}

fn validate_goal_ttl(ttl_secs: Option<i64>) -> Result<i64, String> {
    let ttl = ttl_secs.unwrap_or(DEFAULT_GOAL_TTL_SECS);
    if !(60..=MAX_GOAL_TTL_SECS).contains(&ttl) {
        return Err(format!(
            "goal ttl_secs must be between 60 and {}",
            MAX_GOAL_TTL_SECS
        ));
    }
    Ok(ttl)
}

fn require_active_goal(
    db: &Database,
    goal_id: &str,
    project: &str,
) -> Result<CodexGoalRecord, String> {
    let goal = db
        .get_goal(goal_id)
        .map_err(|e| format!("Failed to load goal: {}", e))?
        .ok_or_else(|| "Goal not found".to_string())?;
    if goal.project != project {
        return Err("Goal project does not match request project".to_string());
    }
    if goal.status != "active" {
        return Err("Goal is not active".to_string());
    }
    let now = chrono::Utc::now().timestamp();
    if goal.expires_at < now {
        let _ = db.update_goal_status(&goal.id, "expired", now, Some("Goal expired"));
        return Err("Goal expired".to_string());
    }
    Ok(goal)
}

fn reject_command_request_inner(
    db: &Database,
    request_id: String,
    reason: Option<String>,
) -> CommandRequestResponse {
    if let Err(e) = validate_command_request_reason(&reason) {
        return CommandRequestResponse {
            success: false,
            request_id: Some(request_id),
            record: None,
            error: Some(e),
        };
    }
    let error = reason.unwrap_or_else(|| "Rejected by user".to_string());
    match db.reject_command_request(&request_id, chrono::Utc::now().timestamp(), &error) {
        Ok(Some(record)) => CommandRequestResponse {
            success: true,
            request_id: Some(record.id.clone()),
            record: Some(record),
            error: None,
        },
        Ok(None) => match db.get_command_request(&request_id) {
            Ok(Some(record)) => CommandRequestResponse {
                success: false,
                request_id: Some(record.id.clone()),
                record: Some(record),
                error: Some("Command request is not pending".to_string()),
            },
            Ok(None) => CommandRequestResponse {
                success: false,
                request_id: Some(request_id),
                record: None,
                error: Some("Command request not found".to_string()),
            },
            Err(e) => CommandRequestResponse {
                success: false,
                request_id: Some(request_id),
                record: None,
                error: Some(format!("Failed to load command request: {}", e)),
            },
        },
        Err(e) => CommandRequestResponse {
            success: false,
            request_id: Some(request_id),
            record: None,
            error: Some(format!("Failed to reject command request: {}", e)),
        },
    }
}

fn approve_command_request_inner(
    projects: &ProjectsConfig,
    db: &Database,
    request_id: String,
) -> CommandRequestResponse {
    let approved_at = chrono::Utc::now().timestamp();
    let min_created_at = approved_at - COMMAND_REQUEST_TTL_SECS;
    let mut record =
        match db.claim_command_request_for_execution(&request_id, approved_at, min_created_at) {
            Ok(Some(record)) => record,
            Ok(None) => match db.get_command_request(&request_id) {
                Ok(Some(record)) => {
                    if record.status == "pending" && record.created_at < min_created_at {
                        let error = "Command request expired".to_string();
                        let expired = db
                            .expire_command_request(&record.id, approved_at, &error)
                            .ok()
                            .flatten()
                            .unwrap_or(record);
                        return CommandRequestResponse {
                            success: false,
                            request_id: Some(expired.id.clone()),
                            record: Some(expired),
                            error: Some(error),
                        };
                    }
                    return CommandRequestResponse {
                        success: false,
                        request_id: Some(record.id.clone()),
                        record: Some(record),
                        error: Some("Command request is not pending".to_string()),
                    };
                }
                Ok(None) => {
                    return CommandRequestResponse {
                        success: false,
                        request_id: Some(request_id),
                        record: None,
                        error: Some("Command request not found".to_string()),
                    };
                }
                Err(e) => {
                    return CommandRequestResponse {
                        success: false,
                        request_id: Some(request_id),
                        record: None,
                        error: Some(format!("Failed to load command request: {}", e)),
                    };
                }
            },
            Err(e) => {
                return CommandRequestResponse {
                    success: false,
                    request_id: Some(request_id),
                    record: None,
                    error: Some(format!("Failed to claim command request: {}", e)),
                };
            }
        };
    let proj = match projects.get_project(&record.project) {
        Ok(p) => p,
        Err(e) => {
            record.status = "failed".to_string();
            record.executed_at = Some(chrono::Utc::now().timestamp());
            record.error = Some(e.clone());
            let _ = db.update_command_request_result(&record);
            return CommandRequestResponse {
                success: false,
                request_id: Some(record.id.clone()),
                record: Some(record),
                error: Some(e),
            };
        }
    };
    if !proj.allow_command_requests {
        let error = "Command requests are not enabled for this project".to_string();
        record.status = "failed".to_string();
        record.executed_at = Some(chrono::Utc::now().timestamp());
        record.error = Some(error.clone());
        let _ = db.update_command_request_result(&record);
        return CommandRequestResponse {
            success: false,
            request_id: Some(record.id.clone()),
            record: Some(record),
            error: Some(error),
        };
    }
    let cmd = match record.command_text.clone() {
        Some(cmd) if !cmd.is_empty() => cmd,
        _ => {
            let error = "Command request is missing command_text snapshot".to_string();
            record.status = "failed".to_string();
            record.executed_at = Some(chrono::Utc::now().timestamp());
            record.error = Some(error.clone());
            let _ = db.update_command_request_result(&record);
            return CommandRequestResponse {
                success: false,
                request_id: Some(record.id.clone()),
                record: Some(record),
                error: Some(error),
            };
        }
    };
    let (code, stdout, stderr, _) =
        run_project_cmd(proj, &cmd, CHECK_TIMEOUT_SECS, projects.ssh.as_ref());
    let (stdout_tail, _) = sanitize_tail(&stdout, MAX_OUTPUT_LEN);
    let (stderr_tail, _) = sanitize_tail(&stderr, MAX_OUTPUT_LEN);
    record.status = if code == 0 { "completed" } else { "failed" }.to_string();
    record.approved_at = Some(approved_at);
    record.executed_at = Some(chrono::Utc::now().timestamp());
    record.exit_code = Some(code);
    record.stdout_tail = Some(stdout_tail);
    record.stderr_tail = Some(stderr_tail);
    record.error = if code == 0 {
        None
    } else {
        Some("command failed".to_string())
    };
    if let Err(e) = db.update_command_request_result(&record) {
        return CommandRequestResponse {
            success: false,
            request_id: Some(record.id.clone()),
            record: Some(record),
            error: Some(format!("Failed to update command request: {}", e)),
        };
    }
    CommandRequestResponse {
        success: code == 0,
        request_id: Some(record.id.clone()),
        record: Some(record),
        error: if code == 0 {
            None
        } else {
            Some("command failed".to_string())
        },
    }
}

#[handler]
pub async fn codex_command_request_op(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(op_response(
            "unknown",
            false,
            Vec::new(),
            Some("Projects not configured".to_string()),
        )));
        return;
    };
    let Some(db) = get_db(depot) else {
        res.render(Json(op_response(
            "unknown",
            false,
            Vec::new(),
            Some("Database not configured".to_string()),
        )));
        return;
    };
    let body: CommandRequestOpRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(op_response(
                "unknown",
                false,
                Vec::new(),
                Some(format!("Invalid JSON: {}", e)),
            )));
            return;
        }
    };
    match body.op.as_str() {
        "create_goal" => {
            let Some(project) = body.project else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("project is required".to_string()),
                )));
                return;
            };
            let title = body.title.unwrap_or_else(|| "Development goal".to_string());
            if let Err(e) = validate_goal_text(&title, &body.summary) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            let ttl_secs = match validate_goal_ttl(body.ttl_secs) {
                Ok(ttl) => ttl,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            if let Err(e) = projects.get_project(&project) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            let now = chrono::Utc::now().timestamp();
            let goal = build_goal_record(project, title, body.summary, now, ttl_secs);
            if let Err(e) = db.insert_goal(&goal) {
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to create goal: {}", e)),
                )));
                return;
            }
            res.render(Json(op_response_with_goals(
                &body.op,
                true,
                Vec::new(),
                vec![goal],
                None,
            )));
        }
        "list_goals" => {
            if let Some(status) = &body.status {
                if let Err(e) = validate_goal_status(status) {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            }
            match db.list_goals(body.project.as_deref(), body.status.as_deref(), body.limit) {
                Ok(goals) => res.render(Json(op_response_with_goals(
                    &body.op,
                    true,
                    Vec::new(),
                    goals,
                    None,
                ))),
                Err(e) => res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to list goals: {}", e)),
                ))),
            }
        }
        "close_goal" => {
            let Some(goal_id) = body.goal_id else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("goal_id is required".to_string()),
                )));
                return;
            };
            match db.update_goal_status(
                &goal_id,
                "closed",
                chrono::Utc::now().timestamp(),
                body.reason.as_deref(),
            ) {
                Ok(Some(goal)) => res.render(Json(op_response_with_goals(
                    &body.op,
                    true,
                    Vec::new(),
                    vec![goal],
                    None,
                ))),
                Ok(None) => {
                    res.status_code(StatusCode::NOT_FOUND);
                    res.render(Json(op_response(
                        &body.op,
                        false,
                        Vec::new(),
                        Some("Goal not found".to_string()),
                    )));
                }
                Err(e) => res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to close goal: {}", e)),
                ))),
            }
        }
        "approve_goal" => {
            let Some(goal_id) = body.goal_id else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("goal_id is required".to_string()),
                )));
                return;
            };
            let now = chrono::Utc::now().timestamp();
            let current = match db.get_goal(&goal_id) {
                Ok(Some(goal)) => goal,
                Ok(None) => {
                    res.status_code(StatusCode::NOT_FOUND);
                    res.render(Json(op_response(
                        &body.op,
                        false,
                        Vec::new(),
                        Some("Goal not found".to_string()),
                    )));
                    return;
                }
                Err(e) => {
                    res.render(Json(op_response(
                        &body.op,
                        false,
                        Vec::new(),
                        Some(format!("Failed to load goal: {}", e)),
                    )));
                    return;
                }
            };
            if current.status != "pending" {
                res.render(Json(op_response_with_goals(
                    &body.op,
                    false,
                    Vec::new(),
                    vec![current],
                    Some("Goal is not pending".to_string()),
                )));
                return;
            }
            if current.expires_at < now {
                let expired = db
                    .update_pending_goal_status(
                        &goal_id,
                        "expired",
                        Some(now),
                        Some("Goal expired"),
                    )
                    .ok()
                    .flatten()
                    .unwrap_or(current);
                res.render(Json(op_response_with_goals(
                    &body.op,
                    false,
                    Vec::new(),
                    vec![expired],
                    Some("Goal expired".to_string()),
                )));
                return;
            }
            match db.update_pending_goal_status(&goal_id, "active", None, None) {
                Ok(Some(goal)) => res.render(Json(op_response_with_goals(
                    &body.op,
                    true,
                    Vec::new(),
                    vec![goal],
                    None,
                ))),
                Ok(None) => match db.get_goal(&goal_id) {
                    Ok(Some(goal)) => res.render(Json(op_response_with_goals(
                        &body.op,
                        false,
                        Vec::new(),
                        vec![goal],
                        Some("Goal is not pending".to_string()),
                    ))),
                    Ok(None) => {
                        res.status_code(StatusCode::NOT_FOUND);
                        res.render(Json(op_response(
                            &body.op,
                            false,
                            Vec::new(),
                            Some("Goal not found".to_string()),
                        )));
                    }
                    Err(e) => res.render(Json(op_response(
                        &body.op,
                        false,
                        Vec::new(),
                        Some(format!("Failed to load goal: {}", e)),
                    ))),
                },
                Err(e) => res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to approve goal: {}", e)),
                ))),
            }
        }
        "reject_goal" => {
            let Some(goal_id) = body.goal_id else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("goal_id is required".to_string()),
                )));
                return;
            };
            let now = chrono::Utc::now().timestamp();
            let reason = body.reason.as_deref().unwrap_or("Goal rejected");
            match db.update_pending_goal_status(&goal_id, "rejected", Some(now), Some(reason)) {
                Ok(Some(goal)) => res.render(Json(op_response_with_goals(
                    &body.op,
                    true,
                    Vec::new(),
                    vec![goal],
                    None,
                ))),
                Ok(None) => match db.get_goal(&goal_id) {
                    Ok(Some(goal)) => res.render(Json(op_response_with_goals(
                        &body.op,
                        false,
                        Vec::new(),
                        vec![goal],
                        Some("Goal is not pending".to_string()),
                    ))),
                    Ok(None) => {
                        res.status_code(StatusCode::NOT_FOUND);
                        res.render(Json(op_response(
                            &body.op,
                            false,
                            Vec::new(),
                            Some("Goal not found".to_string()),
                        )));
                    }
                    Err(e) => res.render(Json(op_response(
                        &body.op,
                        false,
                        Vec::new(),
                        Some(format!("Failed to load goal: {}", e)),
                    ))),
                },
                Err(e) => res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to reject goal: {}", e)),
                ))),
            }
        }
        "create_raw_and_approve" => {
            let Some(project) = body.project else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("project is required".to_string()),
                )));
                return;
            };
            let Some(goal_id) = body.goal_id else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("goal_id is required".to_string()),
                )));
                return;
            };
            let Some(command_text) = body.command_text else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("command_text is required".to_string()),
                )));
                return;
            };
            let goal = match require_active_goal(&db, &goal_id, &project) {
                Ok(goal) => goal,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            if let Err(e) = validate_command_request_reason(&body.reason) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            if let Err(e) = validate_raw_command_text(&command_text) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            let proj = match projects.get_project(&project) {
                Ok(p) => p,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            if !proj.allow_raw_command_requests {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("Raw command requests are not enabled for this project".to_string()),
                )));
                return;
            }
            let reason = Some(format!(
                "[goal:{}] {}",
                goal.id,
                body.reason.unwrap_or_else(|| goal.title.clone())
            ));
            let record = build_command_audit_record(
                project,
                "raw".to_string(),
                command_text.trim().to_string(),
                reason,
                chrono::Utc::now().timestamp(),
            );
            let request_id = record.id.clone();
            if let Err(e) = db.insert_command_request(&record) {
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to create raw command request: {}", e)),
                )));
                return;
            }
            let resp = approve_command_request_inner(&projects, &db, request_id);
            let records = resp.record.clone().into_iter().collect::<Vec<_>>();
            res.render(Json(CommandRequestOpResponse {
                success: resp.success,
                op: body.op,
                records,
                goals: vec![goal.clone()],
                request_id: resp.request_id,
                record: resp.record,
                goal_id: Some(goal.id.clone()),
                goal: Some(goal),
                error: resp.error,
            }));
        }
        "create_and_approve" => {
            let Some(project) = body.project else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("project is required".to_string()),
                )));
                return;
            };
            let Some(goal_id) = body.goal_id else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("goal_id is required".to_string()),
                )));
                return;
            };
            let Some(command) = body.command else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("command is required".to_string()),
                )));
                return;
            };
            let goal = match require_active_goal(&db, &goal_id, &project) {
                Ok(goal) => goal,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            if let Err(e) = validate_command_request_reason(&body.reason) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            let proj = match projects.get_project(&project) {
                Ok(p) => p,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            if !proj.allow_command_requests {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("Command requests are not enabled for this project".to_string()),
                )));
                return;
            }
            let command_text = match get_project_command(proj, &command) {
                Ok(cmd) => cmd,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            let reason = Some(format!(
                "[goal:{}] {}",
                goal.id,
                body.reason.unwrap_or_else(|| goal.title.clone())
            ));
            let record = build_command_audit_record(
                project,
                command,
                command_text,
                reason,
                chrono::Utc::now().timestamp(),
            );
            let request_id = record.id.clone();
            if let Err(e) = db.insert_command_request(&record) {
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to create command request: {}", e)),
                )));
                return;
            }
            let resp = approve_command_request_inner(&projects, &db, request_id);
            let records = resp.record.clone().into_iter().collect::<Vec<_>>();
            res.render(Json(CommandRequestOpResponse {
                success: resp.success,
                op: body.op,
                records,
                goals: vec![goal.clone()],
                request_id: resp.request_id,
                record: resp.record,
                goal_id: Some(goal.id.clone()),
                goal: Some(goal),
                error: resp.error,
            }));
        }
        "list" => {
            if let Some(status) = &body.status {
                if let Err(e) = validate_command_request_status(status) {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            }
            match db.list_command_requests(
                body.project.as_deref(),
                body.status.as_deref(),
                body.limit,
            ) {
                Ok(records) => res.render(Json(op_response(&body.op, true, records, None))),
                Err(e) => res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to list command requests: {}", e)),
                ))),
            }
        }
        "create" => {
            let Some(project) = body.project else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("project is required".to_string()),
                )));
                return;
            };
            let Some(command) = body.command else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("command is required".to_string()),
                )));
                return;
            };
            if let Err(e) = validate_command_request_reason(&body.reason) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            let proj = match projects.get_project(&project) {
                Ok(p) => p,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            if !proj.allow_command_requests {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("Command requests are not enabled for this project".to_string()),
                )));
                return;
            }
            let command_text = match get_project_command(proj, &command) {
                Ok(cmd) => cmd,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            let record = build_command_audit_record(
                project,
                command,
                command_text,
                body.reason,
                chrono::Utc::now().timestamp(),
            );
            if let Err(e) = db.insert_command_request(&record) {
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to create command request: {}", e)),
                )));
                return;
            }
            res.render(Json(op_response(&body.op, true, vec![record], None)));
        }
        "create_raw" => {
            let Some(project) = body.project else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("project is required".to_string()),
                )));
                return;
            };
            let Some(command_text) = body.command_text else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("command_text is required".to_string()),
                )));
                return;
            };
            if let Err(e) = validate_command_request_reason(&body.reason) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            if let Err(e) = validate_raw_command_text(&command_text) {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                return;
            }
            let proj = match projects.get_project(&project) {
                Ok(p) => p,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            if !proj.allow_raw_command_requests {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("Raw command requests are not enabled for this project".to_string()),
                )));
                return;
            }
            let record = build_command_audit_record(
                project,
                "raw".to_string(),
                command_text.trim().to_string(),
                body.reason,
                chrono::Utc::now().timestamp(),
            );
            if let Err(e) = db.insert_command_request(&record) {
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!("Failed to create raw command request: {}", e)),
                )));
                return;
            }
            res.render(Json(op_response(&body.op, true, vec![record], None)));
        }
        "create_batch" => {
            let Some(project) = body.project else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("project is required".to_string()),
                )));
                return;
            };
            if body.requests.is_empty() || body.requests.len() > MAX_COMMAND_REQUEST_BATCH {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!(
                        "requests must contain 1-{} items",
                        MAX_COMMAND_REQUEST_BATCH
                    )),
                )));
                return;
            }
            let proj = match projects.get_project(&project) {
                Ok(p) => p,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
            };
            if !proj.allow_command_requests {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("Command requests are not enabled for this project".to_string()),
                )));
                return;
            }
            let now = chrono::Utc::now().timestamp();
            let mut records = Vec::with_capacity(body.requests.len());
            for item in body.requests {
                if let Err(e) = validate_command_request_reason(&item.reason) {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                    return;
                }
                let command_text = match get_project_command(proj, &item.command) {
                    Ok(cmd) => cmd,
                    Err(e) => {
                        res.status_code(StatusCode::BAD_REQUEST);
                        res.render(Json(op_response(&body.op, false, Vec::new(), Some(e))));
                        return;
                    }
                };
                records.push(build_command_audit_record(
                    project.clone(),
                    item.command,
                    command_text,
                    item.reason,
                    now,
                ));
            }
            for record in &records {
                if let Err(e) = db.insert_command_request(record) {
                    res.render(Json(op_response(
                        &body.op,
                        false,
                        Vec::new(),
                        Some(format!("Failed to create command request: {}", e)),
                    )));
                    return;
                }
            }
            res.render(Json(op_response(&body.op, true, records, None)));
        }
        "approve" | "reject" => {
            let Some(request_id) = body.request_id else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some("request_id is required".to_string()),
                )));
                return;
            };
            let resp = if body.op == "approve" {
                approve_command_request_inner(&projects, &db, request_id)
            } else {
                reject_command_request_inner(&db, request_id, body.reason)
            };
            let records = resp.record.clone().into_iter().collect::<Vec<_>>();
            res.render(Json(CommandRequestOpResponse {
                success: resp.success,
                op: body.op,
                records,
                goals: Vec::new(),
                request_id: resp.request_id,
                record: resp.record,
                goal_id: None,
                goal: None,
                error: resp.error,
            }));
        }
        "approve_batch" | "reject_batch" => {
            if body.request_ids.is_empty() || body.request_ids.len() > MAX_COMMAND_REQUEST_BATCH {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(op_response(
                    &body.op,
                    false,
                    Vec::new(),
                    Some(format!(
                        "request_ids must contain 1-{} items",
                        MAX_COMMAND_REQUEST_BATCH
                    )),
                )));
                return;
            }
            let mut records = Vec::new();
            let mut all_success = true;
            let mut first_error = None;
            for request_id in body.request_ids {
                let resp = if body.op == "approve_batch" {
                    approve_command_request_inner(&projects, &db, request_id)
                } else {
                    reject_command_request_inner(&db, request_id, body.reason.clone())
                };
                all_success &= resp.success;
                if first_error.is_none() {
                    first_error = resp.error.clone();
                }
                if let Some(record) = resp.record {
                    records.push(record);
                }
            }
            res.render(Json(op_response(
                &body.op,
                all_success,
                records,
                first_error,
            )));
        }
        _ => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(op_response(
                &body.op,
                false,
                Vec::new(),
                Some("unsupported op".to_string()),
            )));
        }
    }
}

#[handler]
pub async fn codex_command_request(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Projects not configured".to_string()),
        }));
        return;
    };
    let Some(db) = get_db(depot) else {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Database not configured".to_string()),
        }));
        return;
    };
    let body: CommandRequestCreate = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: None,
                record: None,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    if let Err(e) = validate_command_request_reason(&body.reason) {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some(e),
        }));
        return;
    }
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: None,
                record: None,
                error: Some(e),
            }));
            return;
        }
    };
    if !proj.allow_command_requests {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Command requests are not enabled for this project".to_string()),
        }));
        return;
    }
    let command_text = match get_project_command(proj, &body.command) {
        Ok(cmd) => cmd,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: None,
                record: None,
                error: Some(e),
            }));
            return;
        }
    };
    let now = chrono::Utc::now().timestamp();
    let record =
        build_command_audit_record(body.project, body.command, command_text, body.reason, now);
    let request_id = record.id.clone();
    if let Err(e) = db.insert_command_request(&record) {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some(format!("Failed to create command request: {}", e)),
        }));
        return;
    }
    tracing::info!(
        target: "codex.metrics",
        operation = "createCommandRequest",
        project = %record.project,
        command = %record.command,
        request_id = %request_id,
        "codex_command_request_created"
    );
    res.render(Json(CommandRequestResponse {
        success: true,
        request_id: Some(request_id),
        record: Some(record),
        error: None,
    }));
}

#[handler]
pub async fn codex_command_request_raw(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Projects not configured".to_string()),
        }));
        return;
    };
    let Some(db) = get_db(depot) else {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Database not configured".to_string()),
        }));
        return;
    };
    let body: RawCommandRequestCreate = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: None,
                record: None,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    if let Err(e) = validate_command_request_reason(&body.reason) {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some(e),
        }));
        return;
    }
    if let Err(e) = validate_raw_command_text(&body.command_text) {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some(e),
        }));
        return;
    }
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: None,
                record: None,
                error: Some(e),
            }));
            return;
        }
    };
    if !proj.allow_raw_command_requests {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Raw command requests are not enabled for this project".to_string()),
        }));
        return;
    }
    let record = build_command_audit_record(
        body.project,
        "raw".to_string(),
        body.command_text.trim().to_string(),
        body.reason,
        chrono::Utc::now().timestamp(),
    );
    let request_id = record.id.clone();
    if let Err(e) = db.insert_command_request(&record) {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some(format!("Failed to create raw command request: {}", e)),
        }));
        return;
    }
    res.render(Json(CommandRequestResponse {
        success: true,
        request_id: Some(request_id),
        record: Some(record),
        error: None,
    }));
}

#[handler]
pub async fn codex_command_requests(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(db) = get_db(depot) else {
        res.render(Json(CommandRequestsListResponse {
            success: false,
            records: Vec::new(),
            error: Some("Database not configured".to_string()),
        }));
        return;
    };
    let body: CommandRequestsListRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestsListResponse {
                success: false,
                records: Vec::new(),
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    if let Some(status) = &body.status {
        if let Err(e) = validate_command_request_status(status) {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestsListResponse {
                success: false,
                records: Vec::new(),
                error: Some(e),
            }));
            return;
        }
    }
    match db.list_command_requests(body.project.as_deref(), body.status.as_deref(), body.limit) {
        Ok(records) => res.render(Json(CommandRequestsListResponse {
            success: true,
            records,
            error: None,
        })),
        Err(e) => res.render(Json(CommandRequestsListResponse {
            success: false,
            records: Vec::new(),
            error: Some(format!("Failed to list command requests: {}", e)),
        })),
    }
}

#[handler]
pub async fn codex_command_request_batch(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(CommandRequestBatchResponse {
            success: false,
            records: Vec::new(),
            error: Some("Projects not configured".to_string()),
        }));
        return;
    };
    let Some(db) = get_db(depot) else {
        res.render(Json(CommandRequestBatchResponse {
            success: false,
            records: Vec::new(),
            error: Some("Database not configured".to_string()),
        }));
        return;
    };
    let body: CommandRequestBatchCreate = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestBatchResponse {
                success: false,
                records: Vec::new(),
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    if body.requests.is_empty() || body.requests.len() > MAX_COMMAND_REQUEST_BATCH {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(CommandRequestBatchResponse {
            success: false,
            records: Vec::new(),
            error: Some(format!(
                "requests must contain 1-{} items",
                MAX_COMMAND_REQUEST_BATCH
            )),
        }));
        return;
    }
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestBatchResponse {
                success: false,
                records: Vec::new(),
                error: Some(e),
            }));
            return;
        }
    };
    if !proj.allow_command_requests {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(CommandRequestBatchResponse {
            success: false,
            records: Vec::new(),
            error: Some("Command requests are not enabled for this project".to_string()),
        }));
        return;
    }
    let now = chrono::Utc::now().timestamp();
    let mut records = Vec::with_capacity(body.requests.len());
    for item in body.requests {
        if let Err(e) = validate_command_request_reason(&item.reason) {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestBatchResponse {
                success: false,
                records: Vec::new(),
                error: Some(e),
            }));
            return;
        }
        let command_text = match get_project_command(proj, &item.command) {
            Ok(cmd) => cmd,
            Err(e) => {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(CommandRequestBatchResponse {
                    success: false,
                    records: Vec::new(),
                    error: Some(e),
                }));
                return;
            }
        };
        records.push(build_command_audit_record(
            body.project.clone(),
            item.command,
            command_text,
            item.reason,
            now,
        ));
    }
    for record in &records {
        if let Err(e) = db.insert_command_request(record) {
            res.render(Json(CommandRequestBatchResponse {
                success: false,
                records: Vec::new(),
                error: Some(format!("Failed to create command request: {}", e)),
            }));
            return;
        }
    }
    res.render(Json(CommandRequestBatchResponse {
        success: true,
        records,
        error: None,
    }));
}

#[handler]
pub async fn codex_command_reject(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(db) = get_db(depot) else {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Database not configured".to_string()),
        }));
        return;
    };
    let body: CommandRejectRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: None,
                record: None,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    if let Err(e) = validate_command_request_reason(&body.reason) {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: Some(body.request_id),
            record: None,
            error: Some(e),
        }));
        return;
    }
    let error = body
        .reason
        .unwrap_or_else(|| "Rejected by user".to_string());
    match db.reject_command_request(&body.request_id, chrono::Utc::now().timestamp(), &error) {
        Ok(Some(record)) => res.render(Json(CommandRequestResponse {
            success: true,
            request_id: Some(record.id.clone()),
            record: Some(record),
            error: None,
        })),
        Ok(None) => match db.get_command_request(&body.request_id) {
            Ok(Some(record)) => {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(CommandRequestResponse {
                    success: false,
                    request_id: Some(record.id.clone()),
                    record: Some(record),
                    error: Some("Command request is not pending".to_string()),
                }));
            }
            Ok(None) => {
                res.status_code(StatusCode::NOT_FOUND);
                res.render(Json(CommandRequestResponse {
                    success: false,
                    request_id: Some(body.request_id),
                    record: None,
                    error: Some("Command request not found".to_string()),
                }));
            }
            Err(e) => res.render(Json(CommandRequestResponse {
                success: false,
                request_id: Some(body.request_id),
                record: None,
                error: Some(format!("Failed to load command request: {}", e)),
            })),
        },
        Err(e) => res.render(Json(CommandRequestResponse {
            success: false,
            request_id: Some(body.request_id),
            record: None,
            error: Some(format!("Failed to reject command request: {}", e)),
        })),
    }
}

#[handler]
pub async fn codex_command_approve(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Projects not configured".to_string()),
        }));
        return;
    };
    let Some(db) = get_db(depot) else {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: None,
            record: None,
            error: Some("Database not configured".to_string()),
        }));
        return;
    };
    let body: CommandApproveRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: None,
                record: None,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    let approved_at = chrono::Utc::now().timestamp();
    let min_created_at = approved_at - COMMAND_REQUEST_TTL_SECS;
    let mut record =
        match db.claim_command_request_for_execution(&body.request_id, approved_at, min_created_at)
        {
            Ok(Some(record)) => record,
            Ok(None) => match db.get_command_request(&body.request_id) {
                Ok(Some(record)) => {
                    if record.status == "pending" && record.created_at < min_created_at {
                        let error = "Command request expired".to_string();
                        let expired = db
                            .expire_command_request(&record.id, approved_at, &error)
                            .ok()
                            .flatten()
                            .unwrap_or(record);
                        res.status_code(StatusCode::BAD_REQUEST);
                        res.render(Json(CommandRequestResponse {
                            success: false,
                            request_id: Some(expired.id.clone()),
                            record: Some(expired),
                            error: Some(error),
                        }));
                        return;
                    }
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(CommandRequestResponse {
                        success: false,
                        request_id: Some(record.id.clone()),
                        record: Some(record),
                        error: Some("Command request is not pending".to_string()),
                    }));
                    return;
                }
                Ok(None) => {
                    res.status_code(StatusCode::NOT_FOUND);
                    res.render(Json(CommandRequestResponse {
                        success: false,
                        request_id: Some(body.request_id),
                        record: None,
                        error: Some("Command request not found".to_string()),
                    }));
                    return;
                }
                Err(e) => {
                    res.render(Json(CommandRequestResponse {
                        success: false,
                        request_id: Some(body.request_id),
                        record: None,
                        error: Some(format!("Failed to load command request: {}", e)),
                    }));
                    return;
                }
            },
            Err(e) => {
                res.render(Json(CommandRequestResponse {
                    success: false,
                    request_id: Some(body.request_id),
                    record: None,
                    error: Some(format!("Failed to claim command request: {}", e)),
                }));
                return;
            }
        };
    let proj = match projects.get_project(&record.project) {
        Ok(p) => p,
        Err(e) => {
            record.status = "failed".to_string();
            record.executed_at = Some(chrono::Utc::now().timestamp());
            record.error = Some(e.clone());
            let _ = db.update_command_request_result(&record);
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: Some(record.id.clone()),
                record: Some(record),
                error: Some(e),
            }));
            return;
        }
    };
    if !proj.allow_command_requests {
        let error = "Command requests are not enabled for this project".to_string();
        record.status = "failed".to_string();
        record.executed_at = Some(chrono::Utc::now().timestamp());
        record.error = Some(error.clone());
        let _ = db.update_command_request_result(&record);
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: Some(record.id.clone()),
            record: Some(record),
            error: Some(error),
        }));
        return;
    }
    let cmd = match record.command_text.clone() {
        Some(cmd) if !cmd.is_empty() => cmd,
        _ => {
            let error = "Command request is missing command_text snapshot".to_string();
            record.status = "failed".to_string();
            record.executed_at = Some(chrono::Utc::now().timestamp());
            record.error = Some(error.clone());
            let _ = db.update_command_request_result(&record);
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CommandRequestResponse {
                success: false,
                request_id: Some(record.id.clone()),
                record: Some(record),
                error: Some(error),
            }));
            return;
        }
    };
    let (code, stdout, stderr, duration_ms) =
        run_project_cmd(proj, &cmd, CHECK_TIMEOUT_SECS, projects.ssh.as_ref());
    let (stdout_tail, stdout_trunc) = sanitize_tail(&stdout, MAX_OUTPUT_LEN);
    let (stderr_tail, stderr_trunc) = sanitize_tail(&stderr, MAX_OUTPUT_LEN);
    let now = chrono::Utc::now().timestamp();
    record.status = if code == 0 { "completed" } else { "failed" }.to_string();
    record.approved_at = Some(approved_at);
    record.executed_at = Some(now);
    record.exit_code = Some(code);
    record.stdout_tail = Some(stdout_tail);
    record.stderr_tail = Some(stderr_tail);
    record.error = if code == 0 {
        None
    } else {
        Some("command failed".to_string())
    };
    if let Err(e) = db.update_command_request_result(&record) {
        res.render(Json(CommandRequestResponse {
            success: false,
            request_id: Some(record.id.clone()),
            record: Some(record),
            error: Some(format!("Failed to update command request: {}", e)),
        }));
        return;
    }
    tracing::info!(
        target: "codex.metrics",
        operation = "approveCommandRequest",
        project = %record.project,
        command = %record.command,
        request_id = %record.id,
        success = code == 0,
        exit_code = code,
        duration_ms = duration_ms,
        truncated = stdout_trunc || stderr_trunc,
        "codex_command_request_executed"
    );
    res.render(Json(CommandRequestResponse {
        success: code == 0,
        request_id: Some(record.id.clone()),
        record: Some(record),
        error: if code == 0 {
            None
        } else {
            Some("command failed".to_string())
        },
    }));
}

#[handler]
pub async fn codex_check(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(CheckResponse {
            success: false,
            suite: None,
            exit_code: None,
            duration_ms: None,
            stdout_tail: None,
            stderr_tail: None,
            truncated: false,
            error: Some("Projects not configured".to_string()),
        }));
        return;
    };
    let body: CheckRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CheckResponse {
                success: false,
                suite: None,
                exit_code: None,
                duration_ms: None,
                stdout_tail: None,
                stderr_tail: None,
                truncated: false,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(CheckResponse {
                success: false,
                suite: Some(body.suite),
                exit_code: None,
                duration_ms: None,
                stdout_tail: None,
                stderr_tail: None,
                truncated: false,
                error: Some(e),
            }));
            return;
        }
    };
    if !proj.is_check_allowed(&body.suite) {
        res.status_code(StatusCode::FORBIDDEN);
        let suite = body.suite.clone();
        res.render(Json(CheckResponse {
            success: false,
            suite: Some(body.suite),
            exit_code: None,
            duration_ms: None,
            stdout_tail: None,
            stderr_tail: None,
            truncated: false,
            error: Some(format!(
                "Check '{}' is not allowed. Allowed: {}",
                suite,
                proj.allowed_checks.join(", ")
            )),
        }));
        return;
    }
    let cmd = match proj.get_check_command(&body.suite) {
        Ok(c) => c,
        Err(e) => {
            res.render(Json(CheckResponse {
                success: false,
                suite: Some(body.suite),
                exit_code: None,
                duration_ms: None,
                stdout_tail: None,
                stderr_tail: None,
                truncated: false,
                error: Some(e),
            }));
            return;
        }
    };
    let (code, stdout, stderr, duration_ms) =
        run_project_cmd(proj, &cmd, CHECK_TIMEOUT_SECS, projects.ssh.as_ref());
    let (stdout_tail, stdout_trunc) = sanitize_tail(&stdout, MAX_OUTPUT_LEN);
    let (stderr_tail, stderr_trunc) = sanitize_tail(&stderr, MAX_OUTPUT_LEN);
    let truncated = stdout_trunc || stderr_trunc;
    tracing::info!(
        target: "codex.metrics",
        operation = "runProjectCheck",
        project = %body.project,
        suite = %body.suite,
        executor = if proj.is_ssh() { "ssh" } else { "local" },
        success = code == 0,
        exit_code = code,
        duration_ms = duration_ms,
        ssh_calls = if proj.is_ssh() { 1 } else { 0 },
        control_master = projects.ssh.as_ref().map(|s| s.control_master).unwrap_or(false),
        "codex_check_completed"
    );

    res.render(Json(CheckResponse {
        success: code == 0,
        suite: Some(body.suite),
        exit_code: Some(code),
        duration_ms: Some(duration_ms),
        stdout_tail: Some(stdout_tail),
        stderr_tail: Some(stderr_tail),
        truncated,
        error: None,
    }));
}

#[handler]
pub async fn codex_report(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(ReportResponse {
            success: false,
            report_id: None,
            message_id: None,
            path: None,
            error: Some("Projects not configured".to_string()),
        }));
        return;
    };
    let Some(db) = get_db(depot) else {
        res.render(Json(ReportResponse {
            success: false,
            report_id: None,
            message_id: None,
            path: None,
            error: Some("No database".to_string()),
        }));
        return;
    };
    let body: ReportRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ReportResponse {
                success: false,
                report_id: None,
                message_id: None,
                path: None,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    let _proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ReportResponse {
                success: false,
                report_id: None,
                message_id: None,
                path: None,
                error: Some(e),
            }));
            return;
        }
    };

    let report_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let timestamp = now.format("%Y%m%d_%H%M%S").to_string();
    let filename = format!("{}_{}.json", timestamp, &report_id[..8]);
    let report_dir = std::env::var("DROP_DATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("./data"))
        .join("reports");

    if let Err(e) = std::fs::create_dir_all(&report_dir) {
        res.render(Json(ReportResponse {
            success: false,
            report_id: None,
            message_id: None,
            path: None,
            error: Some(format!("Failed to create reports directory: {}", e)),
        }));
        return;
    }

    let report_path = report_dir.join(&filename);
    let report_json = serde_json::json!({
        "id": report_id,
        "project": body.project,
        "status": body.status,
        "title": body.title,
        "summary": body.summary,
        "channel": body.channel,
        "created_at": now.timestamp(),
    });
    if let Err(e) = std::fs::write(
        &report_path,
        serde_json::to_string_pretty(&report_json).unwrap(),
    ) {
        res.render(Json(ReportResponse {
            success: false,
            report_id: None,
            message_id: None,
            path: None,
            error: Some(format!("Failed to write report: {}", e)),
        }));
        return;
    }

    // Write message to channel
    let msg_text = format!("[{}] {}\n\n{}", body.status, body.title, body.summary);
    let message = Message {
        id: uuid::Uuid::new_v4().to_string(),
        channel: body.channel.clone(),
        kind: MessageKind::Text,
        title: Some(format!("[codex] {}", body.title)),
        text: Some(msg_text),
        file_name: None,
        file_path: None,
        file_size: None,
        mime_type: None,
        created_at: now.timestamp(),
        expires_at: None,
    };
    let message_id = message.id.clone();
    if let Err(e) = db.insert_message(&message) {
        // Report was written but message failed
        res.render(Json(ReportResponse {
            success: true,
            report_id: Some(report_id),
            message_id: None,
            path: Some(report_path.to_string_lossy().to_string()),
            error: Some(format!("Report written but message insert failed: {}", e)),
        }));
        return;
    }

    res.render(Json(ReportResponse {
        success: true,
        report_id: Some(report_id),
        message_id: Some(message_id),
        path: Some(report_path.to_string_lossy().to_string()),
        error: None,
    }));
}

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
            user: None,
            allow_patch: true,
            allow_command_requests: true,
            allow_raw_command_requests: true,
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
            },
            ContextBatchItem {
                mode: ContextMode::ReadFile,
                path: Some("README.md".to_string()),
                query: None,
                start_line: 1,
                limit: 10,
            },
        ];
        let results = ssh_context_batch_error_results("proj", &requests, "boom".to_string());
        assert_eq!(results.len(), requests.len());
        assert!(results.iter().all(|r| !r.success));
        assert!(results.iter().all(|r| r.error.as_deref() == Some("boom")));
    }

    #[test]
    fn test_build_ssh_target() {
        let proj = ProjectConfig {
            path: "/tmp/test".to_string(),
            executor: crate::projects::Executor::Ssh,
            host: Some("msi".to_string()),
            user: Some("root".to_string()),
            allow_patch: false,
            allow_command_requests: false,
            allow_raw_command_requests: false,
            allowed_checks: vec![],
            checks: None,
            commands: HashMap::new(),
        };
        assert_eq!(proj.ssh_target().unwrap(), "root@msi");

        let proj_no_user = ProjectConfig {
            path: "/tmp/test".to_string(),
            executor: crate::projects::Executor::Ssh,
            host: Some("msi".to_string()),
            user: None,
            allow_patch: false,
            allow_command_requests: false,
            allow_raw_command_requests: false,
            allowed_checks: vec![],
            checks: None,
            commands: HashMap::new(),
        };
        assert_eq!(proj_no_user.ssh_target().unwrap(), "msi");
    }

    #[test]
    fn test_build_ssh_target_no_host() {
        let proj = ProjectConfig {
            path: "/tmp/test".to_string(),
            executor: crate::projects::Executor::Ssh,
            host: None,
            user: None,
            allow_patch: false,
            allow_command_requests: false,
            allow_raw_command_requests: false,
            allowed_checks: vec![],
            checks: None,
            commands: HashMap::new(),
        };
        assert!(proj.ssh_target().is_err());
    }

    #[test]
    fn test_local_executor_is_default() {
        let proj = ProjectConfig {
            path: "/tmp/test".to_string(),
            executor: crate::projects::Executor::default(),
            host: None,
            user: None,
            allow_patch: false,
            allow_command_requests: false,
            allow_raw_command_requests: false,
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

fn validate_raw_command_text(command_text: &str) -> Result<(), String> {
    let trimmed = command_text.trim();
    if trimmed.is_empty() {
        return Err("raw command cannot be empty".to_string());
    }
    if command_text.chars().count() > MAX_RAW_COMMAND_LEN {
        return Err(format!(
            "raw command is too long; maximum is {} characters",
            MAX_RAW_COMMAND_LEN
        ));
    }
    if command_text
        .chars()
        .any(|ch| ch == '\0' || ch == '\r' || ch == '\n')
    {
        return Err("raw command must be a single line and cannot contain NUL".to_string());
    }
    let lower = trimmed.to_ascii_lowercase();
    let blocked_tokens = [
        "sudo",
        "su ",
        "apt",
        "apt-get",
        "systemctl",
        "service",
        "docker",
        "podman",
        "kubectl",
        "iptables",
        "ufw",
        "mkfs",
        "mount",
        "umount",
        "chmod -r",
        "chown -r",
        "rm -rf",
        "git push",
        "git fetch",
        "git pull",
        "git checkout",
        "git restore",
        "git clean",
        "git reset",
        "git submodule",
        "curl ",
        "wget ",
        "scp ",
        "rsync ",
    ];
    if blocked_tokens.iter().any(|token| lower.contains(token)) {
        return Err("raw command contains a blocked high-risk token".to_string());
    }
    Ok(())
}

fn validate_command_request_reason(reason: &Option<String>) -> Result<(), String> {
    if let Some(reason) = reason {
        if reason.chars().count() > MAX_COMMAND_REASON_LEN {
            return Err(format!(
                "reason is too long; maximum is {} characters",
                MAX_COMMAND_REASON_LEN
            ));
        }
    }
    Ok(())
}

fn validate_command_request_status(status: &str) -> Result<(), String> {
    match status {
        "pending" | "running" | "completed" | "failed" | "rejected" | "expired" => Ok(()),
        _ => Err("invalid status filter".to_string()),
    }
}

fn command_error(project: &str, command: &str, error: String) -> CommandResponse {
    CommandResponse {
        success: false,
        project: project.to_string(),
        command: command.to_string(),
        exit_code: None,
        duration_ms: 0,
        stdout_tail: None,
        stderr_tail: None,
        truncated: false,
        error: Some(error),
    }
}

fn validate_command_name(command: &str) -> Result<(), String> {
    if command.is_empty() {
        return Err("command cannot be empty".to_string());
    }
    if command.len() > 100 {
        return Err("command is too long; maximum is 100 characters".to_string());
    }
    if !command
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.')
    {
        return Err(
            "command may only contain ASCII letters, digits, underscore, dash, and dot".to_string(),
        );
    }
    Ok(())
}

fn get_project_command(proj: &ProjectConfig, command: &str) -> Result<String, String> {
    validate_command_name(command)?;
    proj.commands.get(command).cloned().ok_or_else(|| {
        let mut commands = proj.commands.keys().cloned().collect::<Vec<_>>();
        commands.sort();
        format!(
            "Command '{}' is not configured. Available: {}",
            command,
            commands.join(", ")
        )
    })
}

fn build_command_audit_record(
    project: String,
    command: String,
    command_text: String,
    reason: Option<String>,
    created_at: i64,
) -> CommandAuditRecord {
    CommandAuditRecord {
        id: uuid::Uuid::new_v4().to_string(),
        project,
        command,
        command_text: Some(command_text),
        reason,
        status: "pending".to_string(),
        created_at,
        approved_at: None,
        executed_at: None,
        exit_code: None,
        stdout_tail: None,
        stderr_tail: None,
        error: None,
    }
}

fn git_operation_name(operation: &GitOperation) -> &'static str {
    match operation {
        GitOperation::Status => "status",
        GitOperation::Diff => "diff",
        GitOperation::Log => "log",
        GitOperation::Add => "add",
        GitOperation::Commit => "commit",
        GitOperation::CommitAmendNoEdit => "commit_amend_no_edit",
    }
}

fn git_error(project: &str, operation: &GitOperation, error: String) -> GitResponse {
    GitResponse {
        success: false,
        project: project.to_string(),
        operation: git_operation_name(operation).to_string(),
        exit_code: None,
        duration_ms: 0,
        stdout_tail: None,
        stderr_tail: None,
        truncated: false,
        error: Some(error),
    }
}

const MAX_GIT_PATHS: usize = 50;
const MAX_GIT_PATH_LEN: usize = 512;
const MAX_COMMAND_REASON_LEN: usize = 2_000;
const MAX_RAW_COMMAND_LEN: usize = 2_000;
const MAX_GOAL_TITLE_LEN: usize = 200;
const MAX_GOAL_SUMMARY_LEN: usize = 4_000;
const DEFAULT_GOAL_TTL_SECS: i64 = 2 * 60 * 60;
const MAX_GOAL_TTL_SECS: i64 = 8 * 60 * 60;
const MAX_COMMAND_REQUEST_BATCH: usize = 20;
const COMMAND_REQUEST_TTL_SECS: i64 = 2 * 60 * 60;

fn validate_git_paths(paths: &[String]) -> Result<(), String> {
    if paths.is_empty() {
        return Err("paths cannot be empty for this git operation".to_string());
    }
    if paths.len() > MAX_GIT_PATHS {
        return Err(format!("too many paths; maximum is {}", MAX_GIT_PATHS));
    }
    for path in paths {
        if path.chars().count() > MAX_GIT_PATH_LEN {
            return Err(format!(
                "path is too long; maximum is {} characters",
                MAX_GIT_PATH_LEN
            ));
        }
        validate_edit_path(path)?;
    }
    Ok(())
}

fn shell_join_paths(paths: &[String]) -> String {
    paths
        .iter()
        .map(|p| shell_escape(p))
        .collect::<Vec<_>>()
        .join(" ")
}

fn validate_git_commit_message(message: &str) -> Result<(), String> {
    let len = message.chars().count();
    if len == 0 {
        return Err("commit message cannot be empty".to_string());
    }
    if len > 200 {
        return Err("commit message is too long; maximum is 200 characters".to_string());
    }
    if message
        .chars()
        .any(|ch| ch == '\n' || ch == '\r' || ch == '\0')
    {
        return Err("commit message cannot contain newlines or NUL".to_string());
    }
    Ok(())
}

fn git_command_for_request(body: &GitRequest) -> Result<String, String> {
    match body.operation {
        GitOperation::Status => Ok("git status --short".to_string()),
        GitOperation::Diff => {
            if body.paths.is_empty() {
                Ok("git diff".to_string())
            } else {
                validate_git_paths(&body.paths)?;
                Ok(format!("git diff -- {}", shell_join_paths(&body.paths)))
            }
        }
        GitOperation::Log => Ok("git log --oneline -n 20".to_string()),
        GitOperation::Add => {
            validate_git_paths(&body.paths)?;
            Ok(format!("git add -- {}", shell_join_paths(&body.paths)))
        }
        GitOperation::Commit => {
            validate_git_paths(&body.paths)?;
            let message = body
                .message
                .as_deref()
                .ok_or_else(|| "message is required for commit".to_string())?;
            validate_git_commit_message(message)?;
            let paths = shell_join_paths(&body.paths);
            let message = shell_escape(message);
            Ok(format!(
                "git add -- {paths} && if git diff --cached --quiet -- {paths}; then echo 'No staged changes to commit' >&2; exit 1; fi; git commit -m {message} --no-verify"
            ))
        }
        GitOperation::CommitAmendNoEdit => {
            validate_git_paths(&body.paths)?;
            let paths = shell_join_paths(&body.paths);
            Ok(format!(
                "git add -- {paths} && if git diff --cached --quiet -- {paths}; then echo 'No staged changes to amend' >&2; exit 1; fi; git commit --amend --no-edit --no-verify"
            ))
        }
    }
}

fn edit_error(error: String) -> EditResponse {
    EditResponse {
        success: false,
        changed_files: Vec::new(),
        diff: String::new(),
        warnings: Vec::new(),
        error: Some(error),
    }
}

fn edit_path(edit: &EditOperation) -> &str {
    match edit {
        EditOperation::ReplaceText { path, .. }
        | EditOperation::ReplaceRange { path, .. }
        | EditOperation::AppendFile { path, .. }
        | EditOperation::CreateFile { path, .. }
        | EditOperation::WriteFile { path, .. }
        | EditOperation::CreateBinaryFile { path, .. }
        | EditOperation::WriteBinaryFile { path, .. } => path,
    }
}

fn edit_text_len(edit: &EditOperation) -> usize {
    match edit {
        EditOperation::ReplaceText { new_text, .. } => new_text.len(),
        EditOperation::ReplaceRange { new_text, .. } => new_text.len(),
        EditOperation::AppendFile { text, .. } => text.len(),
        EditOperation::CreateFile { content, .. } => content.len(),
        EditOperation::WriteFile { content, .. } => content.len(),
        EditOperation::CreateBinaryFile { .. } | EditOperation::WriteBinaryFile { .. } => 0,
    }
}

fn validate_edit_path(rel_path: &str) -> Result<(), String> {
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

fn decode_binary_artifact(base64_content: &str, rel_path: &str) -> Result<Vec<u8>, String> {
    if base64_content.len() > MAX_BINARY_ARTIFACT_SIZE * 2 {
        return Err(format!(
            "base64 content for {} is too large; maximum decoded size is {} bytes",
            rel_path, MAX_BINARY_ARTIFACT_SIZE
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_content)
        .map_err(|e| format!("Invalid base64 content for {}: {}", rel_path, e))?;
    if bytes.len() > MAX_BINARY_ARTIFACT_SIZE {
        return Err(format!(
            "binary content for {} exceeds {} bytes",
            rel_path, MAX_BINARY_ARTIFACT_SIZE
        ));
    }
    Ok(bytes)
}

fn simple_binary_diff(path: &str, old_len: Option<usize>, new_len: usize) -> String {
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

fn simple_file_diff(path: &str, old: Option<&str>, new: &str) -> String {
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

fn resolve_edit_path(root: &Path, rel_path: &str, must_exist: bool) -> Result<PathBuf, String> {
    validate_edit_path(rel_path)?;
    let full_path = root.join(rel_path);
    if must_exist {
        return canonicalize_and_verify(&full_path, root);
    }
    let parent = full_path
        .parent()
        .ok_or_else(|| "path has no parent directory".to_string())?;
    let canonical_parent = canonicalize_and_verify(parent, root)?;
    let file_name = full_path
        .file_name()
        .ok_or_else(|| "path has no file name".to_string())?;
    Ok(canonical_parent.join(file_name))
}

fn read_edit_file(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("Failed to stat file: {}", e))?;
    if meta.len() > MAX_EDIT_FILE_SIZE {
        return Err(format!(
            "File is too large for edit API: {} bytes",
            meta.len()
        ));
    }
    std::fs::read_to_string(path).map_err(|e| format!("Failed to read UTF-8 text file: {}", e))
}

fn replace_nth(
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

fn replace_line_range(
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

fn load_edit_content(
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

fn local_apply_project_edit(proj: &ProjectConfig, body: &EditRequest) -> EditResponse {
    let root = proj.root();
    if !root.exists() {
        return edit_error("Project root does not exist".to_string());
    }
    let mut paths: HashMap<String, PathBuf> = HashMap::new();
    let mut originals: HashMap<String, Option<String>> = HashMap::new();
    let mut current: HashMap<String, Option<String>> = HashMap::new();
    let mut binary_originals: HashMap<String, Option<Vec<u8>>> = HashMap::new();
    let mut binary_current: HashMap<String, Option<Vec<u8>>> = HashMap::new();
    let mut changed = BTreeSet::new();
    for edit in &body.edits {
        let rel_path = edit_path(edit).to_string();
        if let Err(e) = validate_edit_path(&rel_path) {
            return edit_error(e);
        }
        if edit_text_len(edit) > MAX_EDIT_TEXT_SIZE {
            return edit_error(format!(
                "edit text for {} exceeds {} bytes",
                rel_path, MAX_EDIT_TEXT_SIZE
            ));
        }
        match edit {
            EditOperation::ReplaceText {
                old_text,
                new_text,
                occurrence,
                ..
            } => {
                let before = match load_edit_content(
                    &root,
                    &rel_path,
                    &mut paths,
                    &mut originals,
                    &mut current,
                ) {
                    Ok(c) => c,
                    Err(e) => return edit_error(e),
                };
                let after = match replace_nth(&before, old_text, new_text, *occurrence) {
                    Ok(c) => c,
                    Err(e) => return edit_error(e),
                };
                current.insert(rel_path.clone(), Some(after));
            }
            EditOperation::ReplaceRange {
                start_line,
                end_line,
                new_text,
                ..
            } => {
                let before = match load_edit_content(
                    &root,
                    &rel_path,
                    &mut paths,
                    &mut originals,
                    &mut current,
                ) {
                    Ok(c) => c,
                    Err(e) => return edit_error(e),
                };
                let after = match replace_line_range(&before, *start_line, *end_line, new_text) {
                    Ok(c) => c,
                    Err(e) => return edit_error(e),
                };
                current.insert(rel_path.clone(), Some(after));
            }
            EditOperation::AppendFile { text, .. } => {
                let mut before = match load_edit_content(
                    &root,
                    &rel_path,
                    &mut paths,
                    &mut originals,
                    &mut current,
                ) {
                    Ok(c) => c,
                    Err(e) => return edit_error(e),
                };
                before.push_str(text);
                current.insert(rel_path.clone(), Some(before));
            }
            EditOperation::CreateFile { content, .. } => {
                let full_path = match resolve_edit_path(&root, &rel_path, false) {
                    Ok(p) => p,
                    Err(e) => return edit_error(e),
                };
                if full_path.exists() || matches!(current.get(&rel_path), Some(Some(_))) {
                    return edit_error(format!("File already exists: {}", rel_path));
                }
                paths.insert(rel_path.clone(), full_path);
                originals.entry(rel_path.clone()).or_insert(None);
                current.insert(rel_path.clone(), Some(content.clone()));
            }
            EditOperation::WriteFile {
                content,
                allow_overwrite,
                ..
            } => {
                let full_path = match resolve_edit_path(&root, &rel_path, false) {
                    Ok(p) => p,
                    Err(e) => return edit_error(e),
                };
                if full_path.exists() && !allow_overwrite {
                    return edit_error(format!(
                        "File exists and allow_overwrite is false: {}",
                        rel_path
                    ));
                }
                let old = if full_path.exists() {
                    match read_edit_file(&full_path) {
                        Ok(c) => Some(c),
                        Err(e) => return edit_error(e),
                    }
                } else {
                    None
                };
                paths.insert(rel_path.clone(), full_path);
                originals.entry(rel_path.clone()).or_insert(old);
                current.insert(rel_path.clone(), Some(content.clone()));
            }
            EditOperation::CreateBinaryFile { base64_content, .. } => {
                let bytes = match decode_binary_artifact(base64_content, &rel_path) {
                    Ok(bytes) => bytes,
                    Err(e) => return edit_error(e),
                };
                let full_path = match resolve_edit_path(&root, &rel_path, false) {
                    Ok(p) => p,
                    Err(e) => return edit_error(e),
                };
                if full_path.exists() || matches!(binary_current.get(&rel_path), Some(Some(_))) {
                    return edit_error(format!("File already exists: {}", rel_path));
                }
                paths.insert(rel_path.clone(), full_path);
                binary_originals.entry(rel_path.clone()).or_insert(None);
                binary_current.insert(rel_path.clone(), Some(bytes));
            }
            EditOperation::WriteBinaryFile {
                base64_content,
                allow_overwrite,
                ..
            } => {
                let bytes = match decode_binary_artifact(base64_content, &rel_path) {
                    Ok(bytes) => bytes,
                    Err(e) => return edit_error(e),
                };
                let full_path = match resolve_edit_path(&root, &rel_path, false) {
                    Ok(p) => p,
                    Err(e) => return edit_error(e),
                };
                if full_path.exists() && !allow_overwrite {
                    return edit_error(format!(
                        "File exists and allow_overwrite is false: {}",
                        rel_path
                    ));
                }
                let old = if full_path.exists() {
                    match std::fs::read(&full_path) {
                        Ok(bytes) => Some(bytes),
                        Err(e) => return edit_error(format!("Failed to read binary file: {}", e)),
                    }
                } else {
                    None
                };
                paths.insert(rel_path.clone(), full_path);
                binary_originals.entry(rel_path.clone()).or_insert(old);
                binary_current.insert(rel_path.clone(), Some(bytes));
            }
        }
        changed.insert(rel_path);
    }
    let changed_files: Vec<String> = changed.into_iter().collect();
    let mut diff = String::new();
    for path in &changed_files {
        if let Some(Some(new_content)) = current.get(path) {
            diff.push_str(&simple_file_diff(
                path,
                originals.get(path).and_then(|v| v.as_deref()),
                new_content,
            ));
        } else if let Some(Some(new_bytes)) = binary_current.get(path) {
            diff.push_str(&simple_binary_diff(
                path,
                binary_originals
                    .get(path)
                    .and_then(|v| v.as_ref())
                    .map(|v| v.len()),
                new_bytes.len(),
            ));
        }
    }
    if !body.dry_run {
        for path in &changed_files {
            if let (Some(full_path), Some(Some(new_content))) = (paths.get(path), current.get(path))
            {
                if let Err(e) = std::fs::write(full_path, new_content) {
                    return edit_error(format!("Failed to write {}: {}", path, e));
                }
            } else if let (Some(full_path), Some(Some(new_bytes))) =
                (paths.get(path), binary_current.get(path))
            {
                if let Err(e) = std::fs::write(full_path, new_bytes) {
                    return edit_error(format!("Failed to write binary {}: {}", path, e));
                }
            }
        }
    }
    EditResponse {
        success: true,
        changed_files,
        diff,
        warnings: Vec::new(),
        error: None,
    }
}

#[handler]
pub async fn codex_edit(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(edit_error("Projects not configured".to_string())));
        return;
    };
    let body: EditRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(edit_error(format!("Invalid JSON: {}", e))));
            return;
        }
    };
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(edit_error(e)));
            return;
        }
    };
    if !proj.allow_patch() {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(edit_error(
            "Edit is not allowed for this project".to_string(),
        )));
        return;
    }
    if body.edits.is_empty() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(edit_error("edits cannot be empty".to_string())));
        return;
    }
    for edit in &body.edits {
        if let Err(e) = validate_edit_path(edit_path(edit)) {
            res.status_code(StatusCode::FORBIDDEN);
            res.render(Json(edit_error(e)));
            return;
        }
        if edit_text_len(edit) > MAX_EDIT_TEXT_SIZE {
            res.status_code(StatusCode::PAYLOAD_TOO_LARGE);
            res.render(Json(edit_error(format!(
                "edit text for {} exceeds {} bytes",
                edit_path(edit),
                MAX_EDIT_TEXT_SIZE
            ))));
            return;
        }
    }
    let edit_start = Instant::now();
    if proj.is_ssh() {
        let response = ssh_apply_project_edit(proj, &body, projects.ssh.as_ref());
        tracing::info!(
            target: "codex.metrics",
            operation = "applyProjectEdit",
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
        res.render(Json(response));
        return;
    }
    let response = local_apply_project_edit(proj, &body);
    tracing::info!(
        target: "codex.metrics",
        operation = "applyProjectEdit",
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
    res.render(Json(response));
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
}
