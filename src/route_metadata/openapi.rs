#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum OpenApiConsequence {
    NonConsequential,
    Consequential,
}

impl OpenApiConsequence {
    pub(crate) const fn as_bool(self) -> bool {
        matches!(self, Self::Consequential)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum OpenApiExampleSet {
    None,
    RegisterProject,
    CreateProject,
    JobStatus,
    JobLog,
    ListJobs,
    JobTail,
    ReadProjectFile,
    GitStatus,
    GitDiff,
    GitDiffSummary,
    ListProjectFiles,
    SearchProjectText,
    ApplyUnifiedDiff,
    RunShell,
    GitRestorePaths,
    DiscardUntracked,
    ImportConversationFiles,
    StartProjectShellJob,
    CallRuntimeTool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct OpenApiOperationSpec {
    pub(crate) operation_id: &'static str,
    pub(crate) summary: &'static str,
    pub(crate) description: &'static str,
    pub(crate) request_schema: &'static str,
    pub(crate) response_schema: &'static str,
    pub(crate) consequence: OpenApiConsequence,
    pub(crate) examples: OpenApiExampleSet,
}

const fn operation(
    operation_id: &'static str,
    summary: &'static str,
    description: &'static str,
    request_schema: &'static str,
    response_schema: &'static str,
    consequence: OpenApiConsequence,
    examples: OpenApiExampleSet,
) -> OpenApiOperationSpec {
    OpenApiOperationSpec {
        operation_id,
        summary,
        description,
        request_schema,
        response_schema,
        consequence,
        examples,
    }
}

use OpenApiConsequence::{Consequential, NonConsequential};
use OpenApiExampleSet::*;

pub(super) const LIST_RUNTIME_TOOLS: OpenApiOperationSpec = operation(
    "listRuntimeTools",
    "List runtime tools",
    "Read-only. Full detail returns MCP-compatible tool specs and can be too large for GPT Actions. Prefer callRuntimeTool with tool=tool_manifest for daily discovery; when using listRuntimeTools, pass summary_only=true plus category, features, or limit for bounded discovery.",
    "ToolsListRequest",
    "ToolsListResponse",
    NonConsequential,
    None,
);

pub(super) const LIST_PROJECTS: OpenApiOperationSpec = operation(
    "listProjects",
    "List Runner-registered Projects",
    "Read-only. When a Runner or Project is already known, pass exact client_id/project instead of reading the full registry; query is bounded text filtering over already-visible metadata and summary_only returns a compact workspace-selection projection.",
    "ListProjectsRequest",
    "ToolResult",
    NonConsequential,
    None,
);

pub(super) const REGISTER_PROJECT: OpenApiOperationSpec = operation(
    "registerProject",
    "Register an existing project",
    "Mutation with side effects. Registers an existing directory as a WebCodex project on the selected Runner. Executes on the Runner and is constrained by Runner policy. Requires Bearer auth.",
    "RegisterProjectRequest",
    "ToolResult",
    NonConsequential,
    RegisterProject,
);

pub(super) const CREATE_PROJECT: OpenApiOperationSpec = operation(
    "createProject",
    "Create and register a new project",
    "Mutation with side effects. Creates a new directory, or explicitly adopts an already-existing empty directory, on the selected Runner and registers it as a WebCodex Project. Executes on the Runner and is constrained by Runner policy. Requires Bearer auth.",
    "CreateProjectRequest",
    "ToolResult",
    NonConsequential,
    CreateProject,
);

pub(super) const GET_RUNTIME_STATUS: OpenApiOperationSpec = operation(
    "getRuntimeStatus",
    "Get runtime status",
    "Read-only runtime health/observability with Runner count/online_count/stale_count, project/Job counts, and safe allowlisted effective_config. compact=true compacts this response, not MCP schema discovery. Pass exact client_id for one Runner; omit it for fleet-wide status.",
    "RuntimeStatusRequest",
    "ToolResult",
    NonConsequential,
    None,
);

pub(super) const GET_RUNTIME_JOB_STATUS: OpenApiOperationSpec = operation(
    "getRuntimeJobStatus",
    "Get job status",
    "Read-only. Returns status, timing, and exit metadata for a runtime job. Use this to poll the job_id returned by run_job until status is completed, failed, stopped, or lost.",
    "JobStatusRequest",
    "ToolResult",
    NonConsequential,
    JobStatus,
);

pub(super) const GET_RUNTIME_JOB_LOG: OpenApiOperationSpec = operation(
    "getRuntimeJobLog",
    "Get job log",
    "Read-only. Returns bounded tails, line totals, truncation, cursor, exit status, and detected summary for a job_id. Use cursor.stdout as offset to continue.",
    "JobLogRequest",
    "ToolResult",
    NonConsequential,
    JobLog,
);

pub(super) const LIST_RUNTIME_JOBS: OpenApiOperationSpec = operation(
    "listRuntimeJobs",
    "List runtime jobs",
    "Read-only bounded runtime job summaries. Inside a coding Session, prefer exact project/session_id filters; status combines with them using AND semantics. Filters only reduce caller-visible Jobs and are applied before limit. Never returns stdout/stderr bodies.",
    "ListJobsRequest",
    "ToolResult",
    NonConsequential,
    ListJobs,
);

pub(super) const GET_RUNTIME_JOB_TAIL: OpenApiOperationSpec = operation(
    "getRuntimeJobTail",
    "Get job tail",
    "Read-only bounded stdout/stderr tails for a runtime job. Defaults to a bounded tail so the caller never reads full logs by default. Use the job_id returned by run_job.",
    "JobTailRequest",
    "ToolResult",
    NonConsequential,
    JobTail,
);

pub(super) const READ_PROJECT_FILE: OpenApiOperationSpec = operation(
    "readProjectFile",
    "Read a project file",
    "Read-only. Reads a UTF-8 project file through its owning Runner. Output is bounded; use start_line and limit for pagination. The response carries one text representation only: plain by default or 1-based numbered text when with_line_numbers=true.",
    "ReadProjectFileRequest",
    "ToolResult",
    NonConsequential,
    ReadProjectFile,
);

pub(super) const GET_PROJECT_GIT_STATUS: OpenApiOperationSpec = operation(
    "getProjectGitStatus",
    "Get project git status",
    "Runs `git status --porcelain` in a Runner-registered Project and returns stdout, stderr, and exit_code. Safe read-only project inspection; use before proposing changes or invoking mutation tools.",
    "ProjectIdRequest",
    "ToolResult",
    NonConsequential,
    GitStatus,
);

pub(super) const GET_PROJECT_GIT_DIFF: OpenApiOperationSpec = operation(
    "getProjectGitDiff",
    "Get project git diff",
    "Runs `git diff` in a Runner-registered Project and returns stdout, stderr, and exit_code. Optional `args` scopes paths or adds flags (e.g. [\"--stat\"]). Read-only inspection; routes to the owning Runner.",
    "ProjectGitDiffRequest",
    "ToolResult",
    NonConsequential,
    GitDiff,
);

pub(super) const GET_PROJECT_GIT_DIFF_SUMMARY: OpenApiOperationSpec = operation(
    "getProjectGitDiffSummary",
    "Get project git diff summary",
    "Read-only git diff summary for a Runner-registered Project: `git status --porcelain`, `git diff --stat`, and a parsed changed-file list. Does not modify the worktree. Routes to the owning Runner.",
    "ProjectIdRequest",
    "ToolResult",
    NonConsequential,
    GitDiffSummary,
);

pub(super) const LIST_PROJECT_FILES: OpenApiOperationSpec = operation(
    "listProjectFiles",
    "List project files",
    "Read-only deterministic paged file listing of a Runner-registered Project directory. Returns project-relative paths plus a file/dir kind; optional `path` scopes a subdirectory, `limit` bounds the page, and `offset` resumes from `next_offset`. Routes to the owning Runner.",
    "ListProjectFilesRequest",
    "ToolResult",
    NonConsequential,
    ListProjectFiles,
);

pub(super) const SEARCH_PROJECT_TEXT: OpenApiOperationSpec = operation(
    "searchProjectText",
    "Search project text",
    "Read-only bounded project-text search. Regex is the default; prefer pattern_mode=literal for identifiers, snippets, paths, and other exact text. Results use project-relative paths and 1-based line numbers; optional context is bounded and sensitive/build directories are excluded.",
    "SearchProjectTextRequest",
    "ToolResult",
    NonConsequential,
    SearchProjectText,
);

pub(super) const APPLY_UNIFIED_DIFF: OpenApiOperationSpec = operation(
    "applyUnifiedDiff",
    "Apply a unified diff to a project",
    "External/raw unified-diff mutation only, with side effects; requires Bearer auth and Runner shell capability. Use only when input is already a standard unified diff; ordinary model-generated edits should use callRuntimeTool with tool=apply_text_edits after reading the current file SHA, while contextual or large patch-shaped changes can use tool=apply_patch. Performs bounded preflight; failed preflight is zero-write and post-dispatch uncertainty requires workspace inspection.",
    "ApplyUnifiedDiffRequest",
    "ApplyUnifiedDiffToolResult",
    Consequential,
    ApplyUnifiedDiff,
);

pub(super) const RUN_PROJECT_SHELL_COMMAND: OpenApiOperationSpec = operation(
    "runProjectShellCommand",
    "Run a shell command in a project",
    "Runs a shell command in a Runner-registered Project and returns stdout, stderr, exit_code plus command_started/command_ok/failure_kind/tool_failure. Executable with side effects; requires Bearer auth and Runner shell capability.",
    "RunShellRequest",
    "ToolResult",
    Consequential,
    RunShell,
);

pub(super) const GIT_RESTORE_PATHS: OpenApiOperationSpec = operation(
    "gitRestorePaths",
    "Restore tracked project paths",
    "Mutation with side effects. Runs `git restore -- <paths>` on selected tracked project-relative paths. Does not remove untracked files. Requires Bearer auth and the Runner `structured_process_argv` capability.",
    "GitRestorePathsRequest",
    "ToolResult",
    Consequential,
    GitRestorePaths,
);

pub(super) const DISCARD_UNTRACKED_FILES: OpenApiOperationSpec = operation(
    "discardUntrackedFiles",
    "Discard untracked project files",
    "Mutation with side effects. Runs `git clean -f -- <paths>` only for selected project-relative untracked paths. Requires Bearer auth and the Runner `structured_process_argv` capability.",
    "DiscardUntrackedRequest",
    "ToolResult",
    Consequential,
    DiscardUntracked,
);

pub(super) const IMPORT_CONVERSATION_FILES_TO_PROJECT: OpenApiOperationSpec = operation(
    "importConversationFilesToProject",
    "Import ChatGPT conversation files to a project",
    "Mutation with side effects. Downloads GPT Actions openaiFileIdRefs immediately and saves bounded binary files into a Runner-registered Project. Populate openaiFileIdRefs from current conversation files generated by image generation, user upload, or Code Interpreter; never call with an empty array.",
    "ImportConversationFilesRequest",
    "ImportConversationFilesResponse",
    Consequential,
    ImportConversationFiles,
);

pub(super) const START_PROJECT_SHELL_JOB: OpenApiOperationSpec = operation(
    "startProjectShellJob",
    "Start an async project shell job",
    "Starts an async background shell job in a Runner-registered Project and returns a job_id. Execution with side effects; requires Bearer auth and the Runner async shell job capability. Poll with getRuntimeJobStatus; read output with getRuntimeJobTail or getRuntimeJobLog.",
    "StartProjectShellJobRequest",
    "ToolResult",
    Consequential,
    StartProjectShellJob,
);

pub(super) const CALL_RUNTIME_TOOL: OpenApiOperationSpec = operation(
    "callRuntimeTool",
    "Call runtime tool",
    "Generic/advanced route for model-visible runtime tools. Prefer dedicated actions when they match. For ordinary model-generated file edits after read_file/read_files, use tool=apply_text_edits with the current expected_sha256; use tool=apply_patch when a contextual or large patch-shaped change is clearer. Flatten tool args at top level; params is the canonical non-Action envelope; recording_session_id records wrapper calls.",
    "ToolCallRequest",
    "ToolResult",
    Consequential,
    CallRuntimeTool,
);
