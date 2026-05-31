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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobMetadata {
    pub job_id: String,
    #[serde(default)]
    pub client_request_id: Option<String>,
    pub project: String,
    pub goal_id: String,
    pub command: String,
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
    pub reason: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub max_runtime_secs: i64,
    pub executor: String,
    pub pid: Option<i64>,
    pub exit_code: Option<i32>,
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
    pub allowed_checks: Vec<String>,
    pub configured_checks: Vec<String>,
    pub commands: Vec<String>,
    pub capabilities: ProjectCapabilities,
}

#[derive(Debug, Serialize)]
pub struct InstanceInfo {
    pub package_version: String,
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
