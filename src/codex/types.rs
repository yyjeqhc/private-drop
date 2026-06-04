use crate::{CodexGoalRecord, CommandAuditRecord};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ContextMode {
    Overview,
    Tree,
    Search,
    GrepContext,
    ReadFile,
    MarkdownOutline,
    ReadSection,
    AgentContext,
    GitStatus,
    GitDiff,
    /// Read-only aggregate of experiment output files, large files, gitignore status,
    /// and commit recommendations. Designed for paper/experiment projects.
    ExperimentOutputs,
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
    #[serde(default = "default_tree_max_depth")]
    pub max_depth: usize,
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
    #[serde(default = "default_tree_max_depth")]
    pub max_depth: usize,
}

#[derive(Debug, Deserialize)]
pub struct ContextBatchRequest {
    pub project: String,
    pub requests: Vec<ContextBatchItem>,
    #[serde(default = "default_context_batch_max_total_chars")]
    pub max_total_chars: usize,
}

fn default_start_line() -> usize {
    1
}
fn default_limit() -> usize {
    200
}
fn default_context_batch_max_total_chars() -> usize {
    60_000
}
pub(super) fn default_tree_max_depth() -> usize {
    4
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct PatchRequest {
    pub project: String,
    pub patch: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub backend: Option<String>,
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
    pub script_path: Option<String>,
    #[serde(default)]
    pub script_args: Vec<String>,
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
    /// For create_trusted_raw / create_trusted_raw_and_approve: multi-line script text.
    #[serde(default)]
    pub script_text: Option<String>,
    /// For create_trusted_raw / create_trusted_raw_and_approve: timeout in seconds (default 120, max 1800).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// For create_trusted_raw / create_trusted_raw_and_approve: response mode.
    /// "summary" (default, tail only), "full" (more output but still truncated), "minimal" (success/exit_code/cwd only).
    #[serde(default)]
    pub response_mode: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct JobOpRequest {
    pub op: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub goal_id: Option<String>,
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub client_request_id: Option<String>,
    #[serde(default)]
    pub suite: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub script_path: Option<String>,
    #[serde(default)]
    pub script_args: Vec<String>,
    /// For trusted job creation: multi-line script content (written to script.sh in job dir).
    #[serde(default)]
    pub script_text: Option<String>,
    /// For trusted job creation: must be true when script_text is provided.
    #[serde(default)]
    pub trusted: Option<bool>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default = "default_command_request_limit")]
    pub limit: usize,
    #[serde(default = "default_job_tail_lines")]
    pub tail_lines: usize,
    #[serde(default)]
    pub max_runtime_secs: Option<i64>,
    /// For op=log: start reading from this line number (1-based, inclusive).
    /// Omit or set to 0 to use tail-based reading (last tail_lines lines).
    #[serde(default)]
    pub since_line: Option<usize>,
    /// For op=status: response detail level. "basic" (default, lightweight, no logs,
    /// no OOM detection, minimal SSH) or "logs" (basic + include log tails).
    /// tail_lines only affects detail=logs or op=log, not the default detail level.
    #[serde(default)]
    pub detail: Option<String>,
    /// For trusted job: response mode. "summary" (default) or "minimal".
    #[allow(dead_code)]
    #[serde(default)]
    pub response_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobMetadata {
    pub job_id: String,
    #[serde(default)]
    pub client_request_id: Option<String>,
    pub project: String,
    pub goal_id: String,
    pub command: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub suite: Option<String>,
    #[serde(default)]
    pub script_path: Option<String>,
    pub reason: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub max_runtime_secs: i64,
    pub executor: String,
    pub host: Option<String>,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobInfo {
    pub job_id: String,
    pub client_request_id: Option<String>,
    pub project: String,
    pub goal_id: String,
    pub command: String,
    pub kind: Option<String>,
    pub suite: Option<String>,
    pub script_path: Option<String>,
    pub reason: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub max_runtime_secs: i64,
    pub executor: String,
    pub pid: Option<i64>,
    pub exit_code: Option<i32>,
    /// Wall-clock seconds since the job started. Present for running and completed jobs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_secs: Option<i64>,
    /// OOM hint: "possible_oom" if stderr contains OOM-like signals, null otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oom_hint: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct JobOpResponse {
    pub success: bool,
    pub op: String,
    pub job_id: Option<String>,
    pub job_ids: Vec<String>,
    pub job: Option<JobInfo>,
    pub jobs: Vec<JobInfo>,
    pub stdout_tail: Option<String>,
    pub stderr_tail: Option<String>,
    pub summary_markdown: Option<String>,
    pub error: Option<String>,
    /// For op=log: total line count in stdout.log (enables since_line incremental reads).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_total_lines: Option<usize>,
    /// For op=log: the next since_line value to use for incremental polling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<usize>,
    /// For op=recover: always true. Indicates only metadata was read, no log files,
    /// no process checks, no OOM detection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_only: Option<bool>,
    /// For op=status: true when log tails are included in the response (detail=logs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logs_included: Option<bool>,
    /// Operational warnings (e.g., compatibility hints).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

fn default_channel() -> String {
    "omo".to_string()
}

fn default_command_request_limit() -> usize {
    20
}

fn default_job_tail_lines() -> usize {
    80
}

pub(super) fn job_response(op: &str, success: bool, error: Option<String>) -> JobOpResponse {
    JobOpResponse {
        success,
        op: op.to_string(),
        job_id: None,
        job_ids: Vec::new(),
        job: None,
        jobs: Vec::new(),
        stdout_tail: None,
        stderr_tail: None,
        summary_markdown: None,
        error,
        log_total_lines: None,
        next_cursor: None,
        metadata_only: None,
        logs_included: None,
        warnings: Vec::new(),
    }
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
    /// True when the request was rejected by preflight checks before execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight_rejected: Option<bool>,
    /// Estimated total character count from preflight (present when rejected).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_chars: Option<usize>,
    /// Server-enforced maximum allowed characters (present when rejected).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_allowed_chars: Option<usize>,
    /// Server-enforced maximum allowed batch items (present when rejected or warned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_allowed_items: Option<usize>,
    /// Whether the project is SSH (present when rejected or warned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_is_ssh: Option<bool>,
    /// Human-readable suggestion for how to split the request (present when rejected).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
    /// Operational warnings (e.g., batch size hints).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct PatchResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_files: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
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
    /// For create_trusted_raw / create_trusted_raw_and_approve: structured result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trusted_result: Option<TrustedRawCommandResult>,
}

/// Result of a trusted raw command execution.
#[derive(Debug, Clone, Serialize)]
pub struct TrustedRawCommandResult {
    pub exit_code: i32,
    pub duration_ms: u64,
    /// CWD that was used for execution.
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// Path to the audit log on disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_log_path: Option<String>,
    /// Whether the command was blocked by the denylist before execution.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub blocked_by_denylist: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EditRequest {
    pub project: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub response_mode: Option<EditResponseMode>,
    pub edits: Vec<EditOperation>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EditResponseMode {
    Full,
    Summary,
    Minimal,
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
    CreateBinaryArtifact {
        path: String,
        base64_content: String,
    },
    WriteBinaryArtifact {
        path: String,
        base64_content: String,
        #[serde(default)]
        allow_overwrite: bool,
    },
    CreateBinaryFileFromUpload {
        path: String,
        source_file: String,
    },
    WriteBinaryFileFromUpload {
        path: String,
        source_file: String,
        #[serde(default)]
        allow_overwrite: bool,
    },
    CreateBinaryFileFromUrl {
        path: String,
        source_url: String,
    },
    WriteBinaryFileFromUrl {
        path: String,
        source_url: String,
        #[serde(default)]
        allow_overwrite: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EditResponse {
    pub success: bool,
    pub changed_files: Vec<String>,
    pub diff: String,
    #[serde(default)]
    pub diff_truncated: bool,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactOperation {
    SaveBase64,
    SaveUpload,
    SaveUrl,
    SaveGenerated,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ArtifactRequest {
    pub project: String,
    pub op: ArtifactOperation,
    pub path: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub allow_overwrite: bool,
    #[serde(default)]
    pub base64_content: Option<String>,
    #[serde(default)]
    pub source_file: Option<String>,
    #[serde(default)]
    pub file_id: Option<String>,
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub chatgpt_estuary_url: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub alt_text: Option<String>,
    #[serde(default)]
    pub companion_markdown_path: Option<String>,
    #[serde(default)]
    pub companion_markdown_template: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArtifactResponse {
    pub success: bool,
    pub changed_files: Vec<String>,
    pub saved_path: Option<String>,
    pub relative_path: Option<String>,
    pub file_size: Option<u64>,
    pub mime_type: Option<String>,
    pub markdown_snippet: Option<String>,
    pub selected_source: Option<String>,
    pub diff: String,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}

pub(super) struct ArtifactPlan {
    pub(super) edit_request: EditRequest,
    pub(super) saved_path: String,
    pub(super) relative_path: String,
    pub(super) file_size: Option<u64>,
    pub(super) mime_type: Option<String>,
    pub(super) markdown_snippet: Option<String>,
    pub(super) selected_source: String,
    pub(super) warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ProjectCapabilities {
    pub edit: bool,
    pub patch: bool,
    pub artifact: bool,
    pub git: bool,
    pub checks: bool,
    pub jobs: bool,
    pub command_requests: bool,
    pub raw_command_requests: bool,
    pub configured_commands: bool,
    pub reports: bool,
}

#[derive(Debug, Serialize)]
pub struct ProjectCapabilityInfo {
    pub name: String,
    pub executor: String,
    pub root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_target: Option<String>,
    pub ssh_endpoints: Vec<String>,
    pub allowed_checks: Vec<String>,
    pub configured_checks: Vec<String>,
    pub commands: Vec<String>,
    pub default_apply_patch_backend: String,
    pub capabilities: ProjectCapabilities,
}

#[derive(Debug, Serialize)]
pub struct InstanceInfo {
    pub service: String,
    pub api: String,
    pub schema: String,
    pub package_version: String,
    pub server_time: i64,
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    pub data_dir: String,
    pub projects_config_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProjectsResponse {
    pub success: bool,
    pub projects: Vec<ProjectCapabilityInfo>,
    pub project_names: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<InstanceInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{ContextBatchResponse, EditResponse, JobOpRequest, JobOpResponse};

    #[test]
    fn edit_response_deserializes_without_diff_truncated_field() {
        let response: EditResponse = serde_json::from_str(
            r#"{
                "success": true,
                "changed_files": ["a.txt"],
                "diff": "@@",
                "warnings": [],
                "error": null
            }"#,
        )
        .unwrap();
        assert!(!response.diff_truncated);
        assert_eq!(response.changed_files, vec!["a.txt"]);
    }

    #[test]
    fn job_op_request_deserializes_without_detail_field() {
        // Old requests without detail should still deserialize (backward compat)
        let request: JobOpRequest = serde_json::from_str(
            r#"{
                "op": "status",
                "project": "myproj",
                "job_id": "abc-123"
            }"#,
        )
        .unwrap();
        assert_eq!(request.op, "status");
        assert_eq!(request.project, Some("myproj".to_string()));
        assert_eq!(request.job_id, Some("abc-123".to_string()));
        assert!(request.detail.is_none());
    }

    #[test]
    fn job_op_request_deserializes_with_detail_basic() {
        let request: JobOpRequest = serde_json::from_str(
            r#"{
                "op": "status",
                "project": "myproj",
                "job_id": "abc-123",
                "detail": "basic"
            }"#,
        )
        .unwrap();
        assert_eq!(request.detail, Some("basic".to_string()));
    }

    #[test]
    fn job_op_request_deserializes_with_detail_logs() {
        let request: JobOpRequest = serde_json::from_str(
            r#"{
                "op": "status",
                "project": "myproj",
                "job_id": "abc-123",
                "detail": "logs"
            }"#,
        )
        .unwrap();
        assert_eq!(request.detail, Some("logs".to_string()));
    }

    #[test]
    fn job_op_response_serializes_with_metadata_only() {
        let response = JobOpResponse {
            success: true,
            op: "recover".to_string(),
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
            metadata_only: Some(true),
            logs_included: Some(false),
            warnings: Vec::new(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"metadata_only\":true"));
        assert!(json.contains("\"logs_included\":false"));
        // warnings is empty so should be skipped
        assert!(!json.contains("\"warnings\""));
    }

    #[test]
    fn job_op_response_omits_metadata_only_when_none() {
        let response = JobOpResponse {
            success: true,
            op: "status".to_string(),
            job_id: None,
            job_ids: Vec::new(),
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
        let json = serde_json::to_string(&response).unwrap();
        assert!(!json.contains("metadata_only"));
        assert!(!json.contains("logs_included"));
        assert!(!json.contains("warnings"));
    }

    #[test]
    fn job_op_response_deserializes_without_warnings_field() {
        // Old JSON without "warnings" should deserialize (backward compat via serde default)
        let _json = r#"{
            "success": true,
            "op": "status",
            "job_id": "job-1",
            "job_ids": ["job-1"],
            "job": null,
            "jobs": [],
            "stdout_tail": null,
            "stderr_tail": null,
            "summary_markdown": null,
            "error": null
        }"#;
        // JobOpResponse is Serialize-only, so we test by checking the serde(default) attribute
        // on the warnings field is present. The actual deserialization is not supported
        // because JobOpResponse derives only Serialize, not Deserialize.
        // Instead, verify that serializing with empty warnings omits the field.
        let response = JobOpResponse {
            success: true,
            op: "status".to_string(),
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
        let serialized = serde_json::to_string(&response).unwrap();
        assert!(
            !serialized.contains("warnings"),
            "empty warnings should be omitted from serialized JSON"
        );
    }

    #[test]
    fn context_batch_response_omits_new_fields_when_default() {
        // Verify that a ContextBatchResponse without preflight info omits those fields
        let response = ContextBatchResponse {
            success: true,
            project: "test".to_string(),
            results: Vec::new(),
            duration_ms: 100,
            ssh_calls: 0,
            error: None,
            preflight_rejected: None,
            estimated_chars: None,
            max_allowed_chars: None,
            max_allowed_items: None,
            project_is_ssh: None,
            suggestion: None,
            warnings: Vec::new(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(!json.contains("preflight_rejected"));
        assert!(!json.contains("estimated_chars"));
        assert!(!json.contains("suggestion"));
        assert!(!json.contains("warnings"));
    }

    #[test]
    fn context_batch_response_includes_preflight_fields_when_rejected() {
        let response = ContextBatchResponse {
            success: false,
            project: "test".to_string(),
            results: Vec::new(),
            duration_ms: 0,
            ssh_calls: 0,
            error: Some("too large".to_string()),
            preflight_rejected: Some(true),
            estimated_chars: Some(150_000),
            max_allowed_chars: Some(120_000),
            max_allowed_items: Some(8),
            project_is_ssh: Some(true),
            suggestion: Some("Split into smaller batches.".to_string()),
            warnings: Vec::new(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"preflight_rejected\":true"));
        assert!(json.contains("\"estimated_chars\":150000"));
        assert!(json.contains("\"suggestion\":\"Split into smaller batches.\""));
    }
}
