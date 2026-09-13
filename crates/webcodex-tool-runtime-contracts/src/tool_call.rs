//! Runtime tool call wire/data model and behavioral helpers.
//!
//! This module owns the model-visible tool call enum, parsing by runtime tool
//! name, and the project/session accessors used by dispatch guards and audit
//! logging.

#[cfg(feature = "workspace-checkpoints")]
use super::tool_inputs::CheckpointValidationInput;
use super::tool_inputs::{
    default_true, ApplyFileChangeInput, ExecutionPurpose, ExecutionShell, SessionMode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use webcodex_core::apply_patch_shared::ApplyPatchMatchingMode;
use webcodex_core::job_observation::MAX_JOB_OBSERVATION_TOKEN_LEN;
use webcodex_core::lsp_bridge::{
    CallHierarchyDirection, DEFAULT_CALL_HIERARCHY_DEPTH, DEFAULT_CALL_HIERARCHY_LIMIT,
};
use webcodex_core::plugin::{
    validate_json_value as validate_plugin_json_value,
    validate_provider_id as validate_plugin_provider_id,
    validate_tool_name as validate_plugin_tool_name, PLUGIN_MAX_ARGUMENT_BYTES,
};
use webcodex_core::runner_protocol::ShellScriptLanguage;
use webcodex_core::runtime_contract::{
    validate_project_op_path, DEFAULT_OBSERVE_JOBS_TAIL_LINES,
    GIT_DIFF_HUNKS_CONTINUATION_MAX_BYTES,
};
use webcodex_tool_contracts::{lookup_tool_definition, model_visible_tool_names_csv};
use webcodex_workflow_session::{
    strip_tool_call_expectation_metadata, validate_model_facing_assertion_name,
    validate_model_facing_result_expectation, SessionExecutionContext, SessionMessageKind,
    SessionMessagePriority, SessionMessageStatus, ToolCallRecorderMetadata,
};

pub const TOOL_CALL_TOOL_FIELD: &str = "tool";
pub const TOOL_CALL_PARAMS_FIELD: &str = "params";
pub const TOOL_CALL_WRAPPER_FIELDS: &[&str] = &[TOOL_CALL_TOOL_FIELD, TOOL_CALL_PARAMS_FIELD];

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginToolCall {
    pub action: String,
    #[serde(default)]
    pub runner: Option<String>,
    #[serde(default)]
    pub plugin: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub binding: Option<String>,
    #[serde(default)]
    pub arguments: Option<Value>,
}

impl PluginToolCall {
    fn validate(&self) -> Result<(), String> {
        let valid_runner = |runner: &str| {
            !runner.trim().is_empty()
                && runner.len() <= 128
                && !runner.chars().any(char::is_control)
        };
        if self
            .runner
            .as_deref()
            .is_some_and(|runner| !valid_runner(runner))
        {
            return Err("runner must be a bounded non-empty exact Runner client id".to_string());
        }
        if let Some(plugin) = self.plugin.as_deref() {
            validate_plugin_provider_id(plugin)
                .map_err(|_| "plugin must be a valid bounded provider id".to_string())?;
        }
        if let Some(tool) = self.tool.as_deref() {
            validate_plugin_tool_name(tool)
                .map_err(|_| "tool must be a valid bounded provider-local tool name".to_string())?;
        }
        if let Some(binding) = self.binding.as_deref() {
            let Some(random) = binding.strip_prefix("wc_pbind_") else {
                return Err("binding must be a valid opaque Plugin binding".to_string());
            };
            if random.len() != 32
                || !random
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err("binding must be a valid opaque Plugin binding".to_string());
            }
        }
        if let Some(arguments) = self.arguments.as_ref() {
            if !arguments.is_object() {
                return Err("arguments must be a JSON object".to_string());
            }
            validate_plugin_json_value(arguments, PLUGIN_MAX_ARGUMENT_BYTES, "Plugin arguments")
                .map_err(|_| "arguments exceed Plugin bounds".to_string())?;
        }

        match self.action.as_str() {
            "list" => {
                if self.tool.is_some() || self.binding.is_some() || self.arguments.is_some() {
                    return Err("action=list accepts only optional runner and plugin".to_string());
                }
                if self.plugin.is_some() && self.runner.is_none() {
                    return Err("action=list requires runner when plugin is provided".to_string());
                }
            }
            "check" => {
                if self.runner.is_none()
                    || self.plugin.is_none()
                    || self.tool.is_some()
                    || self.binding.is_some()
                    || self.arguments.is_some()
                {
                    return Err("action=check requires only runner and plugin".to_string());
                }
            }
            "reload" => {
                if self.runner.is_none()
                    || self.plugin.is_some()
                    || self.tool.is_some()
                    || self.binding.is_some()
                    || self.arguments.is_some()
                {
                    return Err("action=reload requires only runner".to_string());
                }
            }
            "describe" => {
                if self.runner.is_none()
                    || self.plugin.is_none()
                    || self.tool.is_none()
                    || self.binding.is_some()
                    || self.arguments.is_some()
                {
                    return Err(
                        "action=describe requires only runner, plugin, and tool".to_string()
                    );
                }
            }
            "call" => {
                if self.binding.is_none()
                    || self.arguments.is_none()
                    || self.runner.is_some()
                    || self.plugin.is_some()
                    || self.tool.is_some()
                {
                    return Err("action=call requires only binding and arguments".to_string());
                }
            }
            _ => {
                return Err(
                    "action must be one of list, check, reload, describe, or call".to_string(),
                )
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SshResourceToolCall {
    pub action: String,
    #[serde(default)]
    pub runner: Option<String>,
    #[serde(default)]
    pub binding: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub default_cwd: Option<String>,
}

impl SshResourceToolCall {
    pub fn validate(&self) -> Result<(), String> {
        // Only classify the closed action vocabulary before specialized
        // governance. Action-specific identity/value validation remains in the
        // SSH gateway after scope/session/permission checks so an unauthorized
        // caller cannot learn whether a binding, Runner, name, or target is valid.
        if matches!(self.action.as_str(), "list" | "register" | "remove") {
            Ok(())
        } else {
            Err("action must be one of list, register, or remove".to_string())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchResultMode {
    Matches,
    FilesWithMatches,
    Count,
}

impl SearchResultMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Matches => "matches",
            Self::FilesWithMatches => "files_with_matches",
            Self::Count => "count",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchPatternMode {
    Regex,
    Literal,
}

impl SearchPatternMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Regex => "regex",
            Self::Literal => "literal",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadFilesItem {
    #[serde(deserialize_with = "deserialize_non_empty_read_path")]
    pub path: String,
    #[serde(default)]
    pub start_line: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SearchProjectTextsQuery {
    pub pattern: String,
    #[serde(default)]
    pub pattern_mode: Option<SearchPatternMode>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub context_before: Option<usize>,
    #[serde(default)]
    pub context_after: Option<usize>,
    #[serde(default)]
    pub include_globs: Option<Vec<String>>,
    #[serde(default)]
    pub exclude_globs: Option<Vec<String>>,
    #[serde(default)]
    pub result_mode: Option<SearchResultMode>,
    #[serde(default)]
    pub timeout_secs: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObserveJobsItem {
    #[serde(deserialize_with = "deserialize_non_empty_job_id")]
    pub job_id: String,
    #[serde(default, deserialize_with = "deserialize_optional_observation_token")]
    pub after_observation_token: Option<String>,
}

fn deserialize_non_empty_read_path<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let path = String::deserialize(deserializer)?;
    if path.trim().is_empty() {
        return Err(serde::de::Error::custom("path must not be empty"));
    }
    Ok(path)
}

fn deserialize_non_empty_job_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let job_id = String::deserialize(deserializer)?;
    if job_id.trim().is_empty() {
        return Err(serde::de::Error::custom("job_id must not be empty"));
    }
    Ok(job_id)
}

fn deserialize_optional_observation_token<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let token = Option::<String>::deserialize(deserializer)?;
    if token
        .as_ref()
        .is_some_and(|token| token.len() > MAX_JOB_OBSERVATION_TOKEN_LEN)
    {
        return Err(serde::de::Error::custom(
            "after_observation_token must not exceed 192 bytes",
        ));
    }
    Ok(token)
}

fn deserialize_optional_git_diff_hunks_continuation<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let token = Option::<String>::deserialize(deserializer)?;
    if token
        .as_ref()
        .is_some_and(|token| token.len() > GIT_DIFF_HUNKS_CONTINUATION_MAX_BYTES)
    {
        return Err(serde::de::Error::custom(
            "continuation exceeds the git_diff_hunks size bound",
        ));
    }
    Ok(token)
}

fn deserialize_read_files_items<'de, D>(deserializer: D) -> Result<Vec<ReadFilesItem>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let items = Vec::<ReadFilesItem>::deserialize(deserializer)?;
    if !(1..=8).contains(&items.len()) {
        return Err(serde::de::Error::custom(
            "items must contain between 1 and 8 entries",
        ));
    }
    Ok(items)
}

fn deserialize_observe_jobs_items<'de, D>(deserializer: D) -> Result<Vec<ObserveJobsItem>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let items = Vec::<ObserveJobsItem>::deserialize(deserializer)?;
    if !(1..=8).contains(&items.len()) {
        return Err(serde::de::Error::custom(
            "items must contain between 1 and 8 entries",
        ));
    }
    let mut job_ids = HashSet::with_capacity(items.len());
    if let Some(duplicate) = items
        .iter()
        .map(|item| item.job_id.as_str())
        .find(|job_id| !job_ids.insert(*job_id))
    {
        return Err(serde::de::Error::custom(format!(
            "duplicate job_id in items: {duplicate}"
        )));
    }
    Ok(items)
}

fn deserialize_observe_jobs_tail_lines<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let tail_lines = usize::deserialize(deserializer)?;
    if tail_lines == 0 {
        return Err(serde::de::Error::custom("tail_lines must be at least 1"));
    }
    Ok(tail_lines)
}

fn deserialize_observe_jobs_wait_secs<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let wait_secs = Option::<u64>::deserialize(deserializer)?;
    if wait_secs == Some(0) {
        return Err(serde::de::Error::custom("wait_secs must be at least 1"));
    }
    Ok(wait_secs)
}

fn default_observe_jobs_tail_lines() -> usize {
    DEFAULT_OBSERVE_JOBS_TAIL_LINES
}

fn default_call_hierarchy_depth() -> usize {
    DEFAULT_CALL_HIERARCHY_DEPTH
}

fn default_call_hierarchy_limit() -> usize {
    DEFAULT_CALL_HIERARCHY_LIMIT
}

fn deserialize_search_project_texts_queries<'de, D>(
    deserializer: D,
) -> Result<Vec<SearchProjectTextsQuery>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let queries = Vec::<SearchProjectTextsQuery>::deserialize(deserializer)?;
    if !(1..=8).contains(&queries.len()) {
        return Err(serde::de::Error::custom(
            "queries must contain between 1 and 8 entries",
        ));
    }
    Ok(queries)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiHostFileRef {
    pub download_url: String,
    pub file_id: String,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerSnapshotRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "tool", content = "params", rename_all = "snake_case")]
pub enum ToolCall {
    /// List registered tool runtime tools.
    ListTools {
        #[serde(default)]
        category: Option<String>,
        #[serde(default)]
        features: Option<String>,
        #[serde(default)]
        summary_only: bool,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Create a bounded task tracking session and return an explicit opaque
    /// session id. Later callers should pass that id explicitly (for example as
    /// REST `recording_session_id` wrapper metadata, tool-specific
    /// `session_id`, or MCP `_session_id`) or bind it as current separately.
    StartSession {
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        mode: SessionMode,
        #[serde(default)]
        deny_write_tools: bool,
        #[serde(default)]
        deny_shell_tools: bool,
        #[serde(default)]
        execution_context: Option<SessionExecutionContext>,
    },

    /// Start a normal coding workflow with practical defaults, or continue one
    /// by `session_id`. This is the canonical coding entry and returns only a
    /// compact startup projection.
    /// `session_id` is explicit business input for the exact Workflow Session
    /// to continue; omission always creates a fresh Session.
    WorkOnProject {
        #[serde(default)]
        project: String,
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        base_ref: Option<String>,
        instruction: String,
        #[serde(default = "default_true")]
        include_project_instructions: bool,
        #[serde(default = "default_true")]
        include_workflow_guidance: bool,
        #[serde(default = "default_true")]
        include_extension_catalog: bool,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Return deterministic finish context for an explicit task session:
    /// changes, workspace hygiene, session/handoff summaries, and bounded
    /// validation-like ledger events. Never calls an LLM.
    FinishCodingTask {
        project: String,
        session_id: String,
        #[serde(default)]
        summary_only: bool,
        #[serde(default)]
        include_diff: Option<bool>,
        #[serde(default)]
        include_workspace: Option<bool>,
        #[serde(default)]
        include_hygiene: Option<bool>,
        #[serde(default)]
        include_handoff: Option<bool>,
        #[serde(default)]
        include_validation_summary: Option<bool>,
    },

    /// Explicitly present the current bounded Work Result for one exact coding Session.
    PresentWorkResult {
        project: String,
        session_id: String,
    },

    /// App-only exact read of the same bounded Work Result projection. This
    /// business session identity is deliberately excluded from generic Session
    /// recording so background App polling cannot mutate the observed ledger.
    WorkResultState {
        project: String,
        session_id: String,
    },

    /// Return a bounded structured summary of recorded session ledger data for
    /// an explicit session id.
    SessionSummary {
        session_id: String,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Replace the complete execution defaults of one known active Workflow
    /// Session after resolving and authorizing its exact project. The
    /// in-memory context/event commit is atomic; ledger persistence is queued.
    UpdateSessionContext {
        project: String,
        session_id: String,
        execution_context: SessionExecutionContext,
    },

    /// Explicitly close a workflow session (`Active → Closed`). Requires an
    /// explicit `session_id`. Idempotent when already closed. Does not archive
    /// or evict the Session.
    CloseSession {
        session_id: String,
    },

    /// Read bounded structured validation evidence already present in an
    /// explicit project-scoped session ledger. Never executes validation,
    /// shell commands, Runner requests, or project file reads.
    ValidationSummary {
        project: String,
        session_id: String,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Post a bounded session-local ledger message for collaboration, progress,
    /// guidance, or design discussion. This is session metadata only.
    PostSessionMessage {
        session_id: String,
        kind: SessionMessageKind,
        message: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        reply_to: Option<String>,
        #[serde(default)]
        priority: SessionMessagePriority,
        #[serde(default)]
        requires_ack: bool,
    },

    /// List session-local ledger messages in stable newest-first order.
    ListSessionMessages {
        session_id: String,
        #[serde(default)]
        kind: Option<SessionMessageKind>,
        #[serde(default)]
        status: Option<SessionMessageStatus>,
        #[serde(default)]
        message_id: Option<String>,
        #[serde(default)]
        reply_to: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Read one exact open todo plus every retained direct reply under one
    /// Session-store snapshot and return an opaque assignment fence.
    GetSessionAssignment {
        session_id: String,
        message_id: String,
    },

    /// Observe only message-state changes after an opaque Session-bound durable
    /// cursor. Without a token this establishes a current baseline and returns
    /// no history. Optional waiting is one bounded wait, never a subscription.
    ObserveSessionMessages {
        session_id: String,
        #[serde(default, deserialize_with = "deserialize_optional_observation_token")]
        after_observation_token: Option<String>,
        #[serde(default)]
        wait_secs: Option<u64>,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Mark a session-local message resolved. Idempotent for already resolved
    /// messages.
    ResolveSessionMessage {
        session_id: String,
        message_id: String,
        #[serde(default)]
        resolution: Option<String>,
    },

    /// Atomically answer and resolve one exact open todo. A bounded caller key
    /// makes uncertain-result retries return the original completion.
    CompleteSessionMessage {
        session_id: String,
        message_id: String,
        answer: String,
        completion_key: String,
        expected_assignment_fence: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        priority: SessionMessagePriority,
        /// Kernel-injected trusted provenance. Never accepted from public JSON.
        #[serde(skip)]
        trusted_recording_session_id: Option<String>,
    },

    /// Return a bounded structured aggregate of session-local ledger discussion.
    SessionDiscussionSummary {
        session_id: String,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Return a bounded structured handoff summary for an explicit session id:
    /// session ledger info, message-board state, recent progress/decisions,
    /// open todos/risks/questions/guidance, recent failed tool calls, and
    /// optional workspace, checkpoint, and ledger-derived validation metadata.
    /// Read-only; never calls an LLM or generates natural-language summaries.
    /// Exposed only through runtime tools / MCP / `callRuntimeTool` (no
    /// dedicated OpenAPI op).
    SessionHandoffSummary {
        session_id: String,
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        include_workspace: Option<bool>,
        #[serde(default)]
        include_checkpoints: Option<bool>,
        #[serde(default)]
        include_validation: Option<bool>,
        #[serde(default)]
        summary_only: bool,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Create a bounded last-known-good workspace checkpoint outside the
    /// project worktree.
    #[cfg(feature = "workspace-checkpoints")]
    WorkspaceCheckpointCreate {
        project: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        note: Option<String>,
        #[serde(default)]
        include_untracked: Option<bool>,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        labels: Vec<String>,
        #[serde(default)]
        validation: Option<CheckpointValidationInput>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// List checkpoint metadata for a project without returning diffs.
    #[cfg(feature = "workspace-checkpoints")]
    WorkspaceCheckpointList {
        project: String,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Show bounded checkpoint metadata and file lists without full diff
    /// content.
    #[cfg(feature = "workspace-checkpoints")]
    WorkspaceCheckpointShow {
        project: String,
        checkpoint_id: String,
        #[serde(default)]
        include_diff_stat: Option<bool>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Restore a workspace checkpoint after explicit confirmation.
    #[cfg(feature = "workspace-checkpoints")]
    WorkspaceCheckpointRestore {
        project: String,
        checkpoint_id: String,
        confirm: bool,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Delete a persisted checkpoint file after explicit confirmation.
    #[cfg(feature = "workspace-checkpoints")]
    WorkspaceCheckpointDelete {
        project: String,
        checkpoint_id: String,
        confirm: bool,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Execute one native process directly from a structured executable and
    /// argv. No shell parser, environment mutation, PTY, or durable handoff is
    /// part of this synchronous v1 contract.
    RunProcess {
        project: String,
        executable: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        stdin: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        sync_wait_secs: Option<u64>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        purpose: Option<ExecutionPurpose>,
    },

    /// Admit one native executable + argv as an explicitly detached durable Job.
    /// The detached supervisor owns the accepted payload tree; ordinary
    /// RunProcess remains unchanged.
    RunDetachedProcess {
        project: String,
        idempotency_key: String,
        executable: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        stdin: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        purpose: Option<ExecutionPurpose>,
    },
    CodingAgentStart {
        project: String,
        provider_id: String,
        idempotency_key: String,
        instruction: String,
        #[serde(default)]
        config: Option<BTreeMap<String, webcodex_core::coding_agent::CodingAgentConfigValue>>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        recording_session_id: Option<String>,
    },
    CodingAgentObserve {
        run_id: String,
        #[serde(default)]
        after_observation_token: Option<String>,
        #[serde(default)]
        wait_secs: Option<u64>,
    },
    CodingAgentCancel {
        run_id: String,
    },

    /// Execute bounded script content transported as typed data and written to
    /// a Runner-owned temporary file. The selected language is explicit and
    /// never inherited from Session default_shell.
    RunScript {
        project: String,
        language: ShellScriptLanguage,
        script: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        stdin: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        sync_wait_secs: Option<u64>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        purpose: Option<ExecutionPurpose>,
    },

    /// Execute a shell command in a project directory (sync, short-lived).
    RunShell {
        project: String,
        command: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        purpose: Option<ExecutionPurpose>,
        #[serde(default)]
        shell: Option<ExecutionShell>,
    },

    /// Open one explicit command-oriented persistent shell for this Workflow
    /// Session. It is not shared with run_shell/run_job and is not a Job.
    OpenSessionShell {
        project: String,
        session_id: String,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        shell: Option<ExecutionShell>,
    },

    /// Execute one framed command in an already-open persistent shell.
    SessionShellExec {
        project: String,
        session_id: String,
        shell_id: String,
        command: String,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        purpose: Option<ExecutionPurpose>,
    },

    /// Read Runner-authoritative lifecycle state for a persistent shell.
    SessionShellStatus {
        project: String,
        session_id: String,
        shell_id: String,
    },

    /// Close a persistent shell and its complete process group. Idempotent for
    /// an already-closed shell retained by the current Server process.
    CloseSessionShell {
        project: String,
        session_id: String,
        shell_id: String,
    },

    /// Apply one bounded Codex *** Begin Patch payload transactionally through the owning Runner.
    ApplyPatch {
        project: String,
        patch: String,
        #[serde(default)]
        dry_run: Option<bool>,
        #[serde(default)]
        matching_mode: Option<ApplyPatchMatchingMode>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Apply one bounded raw standard unified diff after an internal safety/applicability preflight.
    ApplyUnifiedDiff {
        project: String,
        diff: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        deny_sensitive_paths: Option<bool>,
    },

    /// Delete project-relative files only (not directories).
    DeleteProjectFiles {
        project: String,
        paths: Vec<String>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Restore tracked paths with `git restore -- <paths>`.
    GitRestorePaths {
        project: String,
        paths: Vec<String>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Discard selected untracked files with `git clean -f -- <paths>`.
    DiscardUntracked {
        project: String,
        paths: Vec<String>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Commit exactly requested changed paths behind an exact HEAD fence.
    GitCommitPaths {
        project: String,
        expected_head: String,
        paths: Vec<String>,
        message: String,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Run `git status` on a project.
    GitStatus {
        project: String,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Return bounded structured recent git commit history.
    GitLog {
        project: String,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        skip: Option<usize>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Return bounded structured hunks from `git diff`.
    GitDiffHunks {
        project: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        paths: Option<Vec<String>>,
        #[serde(default)]
        max_hunks: Option<usize>,
        #[serde(default)]
        max_hunk_lines: Option<usize>,
        #[serde(default)]
        max_page_bytes: Option<usize>,
        #[serde(default)]
        cached: Option<bool>,
        #[serde(default)]
        base_commit: Option<String>,
        #[serde(default)]
        head_commit: Option<String>,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_git_diff_hunks_continuation"
        )]
        continuation: Option<String>,
    },

    /// Return a deterministic bounded review map for an exact committed range.
    GitReviewSummary {
        project: String,
        base_commit: String,
        head_commit: String,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Run `cargo fmt` in a Runner-registered Rust project.
    CargoFmt {
        project: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        check: Option<bool>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        sync_wait_secs: Option<u64>,
    },

    /// Run `cargo check` in a Runner-registered Rust project.
    CargoCheck {
        project: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        all_targets: Option<bool>,
        #[serde(default)]
        all_features: Option<bool>,
        #[serde(default)]
        no_default_features: Option<bool>,
        #[serde(default)]
        features: Option<String>,
        #[serde(default)]
        package: Option<String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        sync_wait_secs: Option<u64>,
    },

    /// Run `cargo test` in a Runner-registered Rust project.
    CargoTest {
        project: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        filter: Option<String>,
        #[serde(default)]
        all_targets: Option<bool>,
        #[serde(default)]
        all_features: Option<bool>,
        #[serde(default)]
        no_default_features: Option<bool>,
        #[serde(default)]
        features: Option<String>,
        #[serde(default)]
        package: Option<String>,
        #[serde(default)]
        no_run: Option<bool>,
        #[serde(default)]
        require_tests: Option<bool>,
        #[serde(default)]
        min_tests: Option<u64>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        sync_wait_secs: Option<u64>,
    },

    /// Run canonical structured `go test -json` validation with an optional
    /// bounded project-relative package scope.
    GoTest {
        project: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        packages: Option<Vec<String>>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        sync_wait_secs: Option<u64>,
    },

    /// Read up to eight UTF-8 files or file ranges under one bounded call.
    ReadFiles {
        project: String,
        #[serde(deserialize_with = "deserialize_read_files_items")]
        items: Vec<ReadFilesItem>,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        with_line_numbers: Option<bool>,
        #[serde(default)]
        max_result_bytes: Option<usize>,
    },

    /// Fresh bounded discovery of project-scoped Agent Skills. This tool is
    /// model-hidden globally and exposed only by capable Stateless MCP Full
    /// Operator surfaces.
    SkillList {
        project: String,
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        expected_catalog_revision: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Read one bounded UTF-8 text resource from a selected Skill package.
    SkillReadFile {
        project: String,
        skill_id: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        start_line: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        expected_definition_revision: Option<String>,
        #[serde(default)]
        expected_package_revision: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
    },

    SkillVersions {
        project: String,
        skill_key: String,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        session_id: Option<String>,
    },

    SkillInstall {
        project: String,
        skill_key: String,
        artifact_path: String,
        expected_artifact_sha256: String,
        idempotency_key: String,
        #[serde(default)]
        activate: Option<bool>,
        #[serde(default)]
        expected_state_revision: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
    },

    SkillActivate {
        project: String,
        skill_key: String,
        package_revision: String,
        expected_state_revision: String,
        idempotency_key: String,
        #[serde(default)]
        session_id: Option<String>,
    },

    SkillRemoveRevision {
        project: String,
        skill_key: String,
        package_revision: String,
        expected_state_revision: String,
        idempotency_key: String,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Create explicit high-level durable intent/control state without execution authority.
    CreateGoal {
        title: String,
        objective: String,
        idempotency_key: String,
    },

    /// Read one exact caller-owned durable Goal.
    GetGoal {
        goal_id: String,
    },

    /// Build the bounded read-only Goal Plan projection for one explicit App presentation.
    PresentGoalPlan {
        goal_id: String,
    },

    /// App-only exact read of the same bounded Goal Plan projection.
    GoalPlanState {
        goal_id: String,
    },

    /// List caller-visible durable Goals with an optional authoritative lifecycle filter.
    ListGoals {
        #[serde(default)]
        lifecycle: Option<String>,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// CAS-update bounded Goal metadata or its closed lifecycle.
    UpdateGoal {
        goal_id: String,
        expected_revision: i64,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        objective: Option<String>,
        #[serde(default)]
        lifecycle: Option<String>,
        #[serde(default)]
        terminal_reason: Option<String>,
        idempotency_key: String,
    },

    /// Explicitly correlate an owned Goal with an independently authorized AgentTask.
    AssociateGoalAgentTask {
        goal_id: String,
        task_id: String,
        idempotency_key: String,
    },

    /// Explicitly correlate an owned Goal with an independently authorized Workflow Session.
    AssociateGoalWorkflowSession {
        goal_id: String,
        session_id: String,
        idempotency_key: String,
    },

    /// Create explicit durable Agent work independent from communication messages and execution backends.
    CreateAgentTask {
        title: String,
        instruction: String,
        #[serde(default)]
        assignee_agent_id: Option<String>,
        #[serde(default)]
        source_conversation_id: Option<String>,
        #[serde(default)]
        source_message_id: Option<String>,
        #[serde(default)]
        referenced_project_id: Option<String>,
        idempotency_key: String,
    },

    /// List durable AgentTasks owned by the current communication principal.
    ListAgentTasks {
        #[serde(default)]
        assignee_agent_id: Option<String>,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Read one exact owned durable AgentTask and latest Attempt metadata.
    ReadAgentTask {
        task_id: String,
    },

    /// Explicitly assign or reassign an AgentTask to an owned durable Agent.
    AssignAgentTask {
        task_id: String,
        assignee_agent_id: String,
    },

    /// Atomically create one fenced leased Attempt for the current assignee.
    StartAgentTaskAttempt {
        task_id: String,
        assignee_agent_id: String,
        idempotency_key: String,
    },

    /// Select the concrete Agent Endpoint continuation backend for one exact live Attempt.
    StartAgentTaskEndpointContinuation {
        task_id: String,
        attempt_id: String,
        assignee_agent_id: String,
        attempt_fence: String,
        attempt_controller_generation: i64,
    },

    /// Explicitly dispatch the exact latest fenced AgentTaskAttempt to one durable CodingAgentRun.
    StartAgentTaskCodingRun {
        project: String,
        task_id: String,
        attempt_id: String,
        assignee_agent_id: String,
        attempt_fence: String,
        attempt_controller_generation: i64,
        provider_id: String,
        #[serde(default)]
        config: Option<BTreeMap<String, webcodex_core::coding_agent::CodingAgentConfigValue>>,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },

    /// Reconcile the exact durable CodingAgentRun already bound to one AgentTaskAttempt.
    ReconcileAgentTaskCodingRun {
        task_id: String,
        attempt_id: String,
    },

    /// Renew only the exact latest unexpired fenced AgentTaskAttempt.
    HeartbeatAgentTaskAttempt {
        task_id: String,
        attempt_id: String,
        assignee_agent_id: String,
        attempt_fence: String,
        attempt_controller_generation: i64,
    },

    /// Commit exact fenced terminal AgentTaskAttempt truth with independent keyed replay.
    CompleteAgentTaskAttempt {
        task_id: String,
        attempt_id: String,
        assignee_agent_id: String,
        attempt_fence: String,
        attempt_controller_generation: i64,
        outcome: String,
        #[serde(default)]
        terminal_result: Option<String>,
        #[serde(default)]
        terminal_reason: Option<String>,
        completion_key: String,
    },

    /// Create a durable Server-owned Agent identity and mutable self-description card.
    CreateAgentIdentity {
        handle: String,
        display_name: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        specialty_labels: Vec<String>,
        idempotency_key: String,
    },

    /// List Agent identities owned by the current communication principal.
    ListAgentIdentities {
        #[serde(default)]
        agent_id: Option<String>,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// CAS-update mutable Agent Card metadata without changing canonical identity.
    UpdateAgentIdentity {
        agent_id: String,
        expected_profile_revision: i64,
        #[serde(default)]
        handle: Option<String>,
        #[serde(default)]
        display_name: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        specialty_labels: Option<Vec<String>>,
    },

    /// Rotate the server-local continuation Endpoint/controller generation for a durable Agent.
    RotateAgentContinuationEndpoint {
        agent_id: String,
        host: String,
        #[serde(default)]
        client_attachment_id: Option<String>,
        idempotency_key: String,
    },

    /// Compatibility name for server-local continuation Endpoint rotation.
    /// Attach a current Host/Client Endpoint to a durable Agent.
    AttachAgentEndpoint {
        agent_id: String,
        host: String,
        #[serde(default)]
        client_attachment_id: Option<String>,
        idempotency_key: String,
    },

    /// Present one exact Agent/Endpoint continuation controller card. Never infers a target.
    PresentAgentContinuation {
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
    },

    /// App-only bind of one live Host View to an exact freshly attached Endpoint generation.
    AgentContinuationBind {
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
        binding_id: String,
    },

    /// App-only same-Window recovery for one exact naturally expired Endpoint.
    AgentContinuationRecoverEndpoint {
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
        binding_id: String,
    },

    /// App-only exact Host heartbeat plus bounded authoritative state refresh.
    AgentContinuationState {
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
        binding_id: String,
    },

    /// App-only pre-fence acquire through the durable Wake claim state machine.
    AgentContinuationWakeAcquire {
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
        binding_id: String,
    },

    /// App-only crossing of the existing durable dispatch fence immediately before ui/message.
    AgentContinuationWakePrepare {
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
        binding_id: String,
        wake_id: String,
        attempt_id: String,
    },

    /// App-only record of Host dispatch acceptance or conservative post-fence uncertainty.
    AgentContinuationWakeFinish {
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
        binding_id: String,
        wake_id: String,
        attempt_id: String,
        outcome: String,
    },

    /// App-only best-effort withdrawal of one exact process-local Host View binding.
    AgentContinuationUnbind {
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
        binding_id: String,
    },

    /// Detach an Endpoint while preserving the durable Agent.
    DetachAgentEndpoint {
        endpoint_id: String,
    },

    /// Create a durable Conversation with the current Human principal and Agents.
    CreateConversation {
        #[serde(default)]
        title: Option<String>,
        agent_ids: Vec<String>,
        idempotency_key: String,
    },

    /// List Conversations for a Human view or an explicitly attached Agent view.
    ListConversations {
        #[serde(default)]
        agent_id: Option<String>,
        #[serde(default)]
        endpoint_id: Option<String>,
        #[serde(default)]
        expected_controller_generation: Option<i64>,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Read an ordered append-only Conversation transcript page.
    ReadConversation {
        conversation_id: String,
        #[serde(default)]
        agent_id: Option<String>,
        #[serde(default)]
        endpoint_id: Option<String>,
        #[serde(default)]
        expected_controller_generation: Option<i64>,
        #[serde(default)]
        after_seq: Option<i64>,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Atomically append a Message and recipient-specific Agent deliveries.
    PostConversationMessage {
        conversation_id: String,
        body: String,
        #[serde(default)]
        author_agent_id: Option<String>,
        #[serde(default)]
        endpoint_id: Option<String>,
        #[serde(default)]
        expected_controller_generation: Option<i64>,
        #[serde(default)]
        recipient_agent_ids: Option<Vec<String>>,
        #[serde(default)]
        reply_to: Option<String>,
        #[serde(default)]
        idempotency_key: Option<String>,
        #[serde(default)]
        wake_reply_id: Option<String>,
        #[serde(default)]
        reply_operation_index: Option<i64>,
    },

    /// List queued deliveries for an Agent proven by an active Endpoint.
    ListAgentInbox {
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
        #[serde(default)]
        after_delivery_order: Option<i64>,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Mark exact recipient-specific Agent deliveries consumed.
    ConsumeAgentDeliveries {
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
        delivery_ids: Vec<String>,
    },

    /// Verify one exact Agent/Endpoint activation and return bounded current
    /// Conversation, Inbox, Wake, Host-binding, and reply-replay context.
    BootstrapAgentConversation {
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
        #[serde(default)]
        conversation_id: Option<String>,
        #[serde(default)]
        wake_id: Option<String>,
        #[serde(default)]
        activation_idempotency_key: Option<String>,
    },

    /// Consume one exact durable Agent Wake continuation without consuming Inbox deliveries.
    ConsumeAgentWake {
        agent_id: String,
        endpoint_id: String,
        expected_controller_generation: i64,
        wake_id: String,
        consume_token: String,
    },

    /// Search/list explicit durable project Memory. Model-hidden globally and
    /// exposed only by the capable Stateless MCP Full Operator surface.
    MemorySearch {
        project: String,
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        tags: Option<Vec<String>>,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        expected_catalog_revision: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Read one explicit durable project Memory body.
    MemoryRead {
        project: String,
        memory_key: String,
        #[serde(default)]
        expected_revision: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Create or CAS-update explicit durable project Memory guidance.
    MemorySet {
        project: String,
        memory_key: String,
        summary: String,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        priority: Option<String>,
        #[serde(default)]
        bootstrap: Option<bool>,
        #[serde(default)]
        tags: Option<Vec<String>>,
        #[serde(default)]
        expected_revision: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// CAS-delete explicit durable project Memory guidance.
    MemoryDelete {
        project: String,
        memory_key: String,
        expected_revision: String,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Admin-only paginated inventory of durable project Memory scopes.
    MemoryScopeList {
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Admin-only explicit purge of one non-current durable Memory scope.
    MemoryScopePurge {
        memory_scope_id: String,
        expected_catalog_revision: String,
        #[serde(default)]
        confirm: bool,
    },

    /// Start an async background job (long-running commands, codex CLI, etc.).
    RunJob {
        project: String,
        command: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        timeout_secs: Option<i64>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        purpose: Option<ExecutionPurpose>,
        #[serde(default)]
        shell: Option<ExecutionShell>,
    },

    /// Stop a bounded runtime job after explicit confirmation.
    StopJob {
        project: String,
        job_id: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        confirm: bool,
    },

    /// Observe up to eight existing Jobs using one shared bounded wait. Each
    /// item reuses the canonical single-Job observation-token and projection
    /// path; item failures are isolated and no Job is launched or modified.
    ObserveJobs {
        #[serde(deserialize_with = "deserialize_observe_jobs_items")]
        items: Vec<ObserveJobsItem>,
        #[serde(
            default = "default_observe_jobs_tail_lines",
            deserialize_with = "deserialize_observe_jobs_tail_lines"
        )]
        tail_lines: usize,
        #[serde(default, deserialize_with = "deserialize_observe_jobs_wait_secs")]
        wait_secs: Option<u64>,
    },

    /// List files in a Runner-registered project directory (bounded, read-only).
    /// Returns project-relative paths plus a file/dir kind. Routed to the
    /// owning registered Runner via the `file_list` op; the server never reads
    /// the Runner project path directly.
    ListProjectFiles {
        project: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        offset: Option<usize>,
    },

    /// List the project's tracked files from the Git index, with glob
    /// filtering and automatic directory rollup. Unlike `ListProjectFiles`
    /// (one directory, filesystem order) this answers "what is in this
    /// project" in a single bounded call and never descends into ignored
    /// directories such as `.venv` or `target`.
    ListProjectTrackedFiles {
        project: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        globs: Option<Vec<String>>,
        #[serde(default)]
        depth: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        offset: Option<usize>,
    },

    /// Return a deterministic, bounded, metadata-only overview of an
    /// Runner-registered project. The owning Runner scans directory entries;
    /// file contents are never read and no LLM is used.
    ProjectOverview {
        project: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        max_depth: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Run up to eight independent bounded project-text searches under one
    /// project authorization and outer Session event.
    SearchProjectTexts {
        project: String,
        #[serde(deserialize_with = "deserialize_search_project_texts_queries")]
        queries: Vec<SearchProjectTextsQuery>,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        max_result_bytes: Option<usize>,
    },

    /// Read-only model-facing git worktree summary for a project. Reports
    /// branch/head, parsed status counts/files, diff stat, warnings, suggested
    /// next actions, and optional bounded diff hunks. Routed to the owning
    /// agent.
    ShowChanges {
        project: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        include_diff: Option<bool>,
        #[serde(default)]
        max_hunks: Option<usize>,
        #[serde(default)]
        max_hunk_lines: Option<usize>,
        #[serde(default)]
        session_event_limit: Option<usize>,
    },

    /// List bounded runtime Job summaries for caller-visible Runner work.
    /// Never returns stdout/stderr bodies — only metadata (job_id, kind,
    /// status, project, timestamps, exit_code).
    ListJobs {
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Return bounded stdout/stderr tails for a job. Defaults to a bounded tail
    /// so the console never reads full logs by default. When `after_observation_token`
    /// and `wait_secs` are both supplied, this is a single bounded wait (up to
    /// `wait_secs`, 1..=60) until the current opaque Job observation token
    /// differs or the Job becomes terminal; it is never a subscription or streaming
    /// connection.
    JobTail {
        job_id: String,
        #[serde(default)]
        tail_lines: Option<usize>,
        #[serde(default)]
        after_observation_token: Option<String>,
        #[serde(default)]
        wait_secs: Option<u64>,
    },

    /// Write a UTF-8 file in a project via the owning Runner. Creates new files
    /// and, with `overwrite=true` plus the exact current `expected_sha256`,
    /// replaces existing ones without a stale read clobbering concurrent work.
    /// The server never reads the Runner filesystem directly; the write runs as
    /// a native agent file operation.
    WriteProjectFile {
        project: String,
        path: String,
        content: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        overwrite: Option<bool>,
        #[serde(default)]
        expected_sha256: Option<String>,
    },

    /// Write a binary artifact in a project via the owning Runner. The payload is
    /// base64-encoded and decoded by the Runner's native artifact file-op path.
    SaveProjectArtifact {
        project: String,
        path: String,
        content_base64: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        mime_type: Option<String>,
        #[serde(default)]
        overwrite: Option<bool>,
    },

    /// Import host-provided ChatGPT conversation attachments without routing
    /// temporary OpenAI download URLs or raw attachment bytes to the model or Runner.
    ImportConversationFilesToProject {
        project: String,
        #[serde(rename = "openaiFileIdRefs")]
        openai_file_id_refs: Vec<OpenAiHostFileRef>,
        #[serde(default)]
        output_dir: Option<String>,
        #[serde(default)]
        targets: Option<Vec<String>>,
        #[serde(default)]
        overwrite: Option<bool>,
        #[serde(default)]
        session_id: Option<String>,
        /// Internal provenance bit set only by the MCP HTTP adapter after
        /// authenticating the OAuth client registration. Never deserialized
        /// from model/caller arguments and never serialized back out.
        #[serde(skip)]
        trusted_mcp_host_file_import: bool,
    },

    /// Prepare one project artifact for standards-native MCP resource export.
    /// The runtime returns only stable metadata; the MCP transport owns the
    /// short-lived resource handle and complete binary framing.
    ExportProjectArtifact {
        project: String,
        path: String,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Read bounded metadata for a binary project artifact. Zip files are
    /// counted but never extracted.
    ReadProjectArtifactMetadata {
        project: String,
        path: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        allow_missing: Option<bool>,
    },

    /// Read one bounded binary content segment for a project artifact. Returns
    /// base64 for the requested chunk plus full-file sha256 and MIME metadata.
    /// MCP callers may request one complete, size-limited PNG/JPEG/WebP for
    /// native image content framing; that transport-only option is deliberately
    /// not part of the generic REST/GPT Actions schema.
    ReadProjectArtifact {
        project: String,
        path: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        encoding: Option<String>,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        length: Option<usize>,
        #[serde(default)]
        as_image: Option<bool>,
    },

    /// Begin a chunked binary artifact upload bounded to 256 MiB. The agent
    /// creates a project-local temporary upload file and returns an opaque id.
    ArtifactUploadBegin {
        project: String,
        path: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        expected_bytes: Option<usize>,
        #[serde(default)]
        expected_sha256: Option<String>,
        #[serde(default)]
        mime_type: Option<String>,
        #[serde(default)]
        overwrite: Option<bool>,
    },

    /// Append one base64-encoded chunk, at most 1 MiB decoded, to an upload.
    ArtifactUploadChunk {
        project: String,
        path: String,
        upload_id: String,
        offset: usize,
        content_base64: String,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Verify and atomically commit a bounded artifact upload.
    ArtifactUploadFinish {
        project: String,
        path: String,
        upload_id: String,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Abort a bounded artifact upload and remove its temporary files.
    ArtifactUploadAbort {
        project: String,
        path: String,
        upload_id: String,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Apply a bounded transactional batch of edit/create/delete/rename file
    /// changes via the owning Runner. Every existing input file requires a
    /// sha256 precondition and every change is preflighted before the first
    /// mutation. `dry_run` computes the full plan without writing. Exposed only
    /// through runtime tools / MCP / `callRuntimeTool` (no dedicated OpenAPI
    /// operation).
    ApplyTextEdits {
        project: String,
        changes: Vec<ApplyFileChangeInput>,
        #[serde(default)]
        dry_run: Option<bool>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Read-only workspace hygiene inspection. Detects pollution risks before
    /// deployment smoke, model handoff, or real development: dirty worktree,
    /// untracked temporary/smoke/anchor files, cache directories, secret-like
    /// path names, and large untracked files. Never cleans, deletes, restores,
    /// or modifies the project. Never reads file contents, env values, tokens,
    /// or stdout/stderr bodies. Suspicious secret files are identified by
    /// path/name only. Exposed only through runtime tools / MCP /
    /// `callRuntimeTool` (no dedicated OpenAPI op).
    WorkspaceHygieneCheck {
        project: String,
        #[serde(default)]
        max_findings: Option<usize>,
        #[serde(default)]
        include_tracked: Option<bool>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// List all Runner-registered runtime Projects.

    /// Probe Runner-side language-server availability without starting it.
    LspStatus {
        project: String,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Hierarchical document symbols for a project-relative supported source file.
    DocumentSymbols {
        project: String,
        path: String,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Latest bounded language-server diagnostics for a project-relative supported source file.
    DocumentDiagnostics {
        project: String,
        path: String,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Hover information at a 1-based Unicode scalar position.
    Hover {
        project: String,
        path: String,
        line: usize,
        column: usize,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Bounded workspace symbols matching a non-empty query.
    WorkspaceSymbols {
        project: String,
        query: String,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Goto definition at a 1-based Unicode scalar position.
    GotoDefinition {
        project: String,
        path: String,
        line: usize,
        column: usize,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Find references at a 1-based Unicode scalar position.
    FindReferences {
        project: String,
        path: String,
        line: usize,
        column: usize,
        #[serde(default = "default_true")]
        include_declaration: bool,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Bounded incoming/outgoing semantic call hierarchy at a source position.
    CallHierarchy {
        project: String,
        path: String,
        line: usize,
        column: usize,
        #[serde(default)]
        direction: CallHierarchyDirection,
        #[serde(default = "default_call_hierarchy_depth")]
        depth: usize,
        #[serde(default = "default_call_hierarchy_limit")]
        limit: usize,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// List caller-visible Runner targets that advertise a Computer observation capability.
    ComputerListTargets,

    /// Enumerate bounded top-level windows on one exact Runner.
    ComputerListWindows {
        client_id: String,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Enumerate a bounded fresh set of installed applications on one exact Runner.
    ComputerListApplications {
        client_id: String,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Enumerate a bounded fresh set of exact full displays on one Runner.
    ComputerListDisplays {
        client_id: String,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Submit one exact native application launch using a fresh opaque discovery id.
    ComputerLaunchApplication {
        client_id: String,
        application_id: String,
    },

    /// Read the exact Runner's macOS Accessibility trust status without prompting.
    ComputerAccessibilityStatus {
        client_id: String,
    },

    /// Inspect one exact previously listed macOS surface as a bounded AX tree.
    ComputerAccessibilityTree {
        client_id: String,
        surface_id: String,
        #[serde(default)]
        max_depth: Option<usize>,
        #[serde(default)]
        max_nodes: Option<usize>,
    },

    /// Find a bounded set of semantic elements on one exact macOS surface.
    ComputerFindElements {
        client_id: String,
        surface_id: String,
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        subrole: Option<String>,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        focused: Option<bool>,
        #[serde(default)]
        enabled: Option<bool>,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Revalidate one exact observed element and return normalized read-only state.
    ComputerElementState {
        client_id: String,
        surface_id: String,
        element_id: String,
    },

    /// Activate and raise one exact previously observed macOS window surface.
    ComputerActivateWindow {
        client_id: String,
        surface_id: String,
    },

    /// Perform one bounded control action on an exact registered AX element.
    ComputerControl {
        client_id: String,
        surface_id: String,
        element_id: String,
        action: String,
    },

    /// Semantically scroll one exact registered AX element into view.
    ComputerScrollToElement {
        client_id: String,
        surface_id: String,
        element_id: String,
    },

    /// Post one closed navigation/action key to one exact already-focused window.
    ComputerKeyInput {
        client_id: String,
        surface_id: String,
        key: String,
        #[serde(default)]
        modifiers: Option<Vec<String>>,
    },

    /// Read bounded native plain Unicode text from the global clipboard.
    ComputerReadClipboard {
        client_id: String,
    },

    /// Replace the global clipboard with bounded native plain Unicode text.
    ComputerWriteClipboard {
        client_id: String,
        text: String,
    },

    /// Move the native macOS or Windows pointer using one latest unspent full-display snapshot generation.
    ComputerPointerMove {
        client_id: String,
        display_id: String,
        snapshot_generation: u32,
        x: u32,
        y: u32,
    },

    /// Submit one native macOS or Windows single-left-click at a snapshot-fenced display-local coordinate.
    ComputerPointerClick {
        client_id: String,
        display_id: String,
        snapshot_generation: u32,
        x: u32,
        y: u32,
    },

    /// Set bounded text on an already-focused, empty exact registered AX text element.
    ComputerInputText {
        client_id: String,
        surface_id: String,
        element_id: String,
        text: String,
    },

    /// Capture one opaque process-local window surface, optionally narrowed to a bounded region.
    ComputerSnapshot {
        client_id: String,
        surface_id: String,
        #[serde(default)]
        region: Option<ComputerSnapshotRegion>,
        #[serde(default)]
        max_width: Option<u32>,
        #[serde(default)]
        max_height: Option<u32>,
    },

    /// Capture one exact previously discovered full display with optional downscale bounds.
    ComputerSnapshotDisplay {
        client_id: String,
        display_id: String,
        #[serde(default)]
        max_width: Option<u32>,
        #[serde(default)]
        max_height: Option<u32>,
    },

    /// Capture one exact window snapshot and persist it directly as a create-only project artifact.
    ComputerSaveSnapshot {
        project: String,
        path: String,
        client_id: String,
        surface_id: String,
        #[serde(default)]
        region: Option<ComputerSnapshotRegion>,
        #[serde(default)]
        max_width: Option<u32>,
        #[serde(default)]
        max_height: Option<u32>,
        #[serde(default)]
        session_id: Option<String>,
    },

    ListProjects {
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        summary_only: bool,
    },

    /// Register an existing directory as a WebCodex project on a selected
    /// agent. The Runner validates the path against its own policy, writes a
    /// project registration record `<project_registry_dir>/<id>.toml` atomically, and refreshes its local
    /// Project list. The Server refreshes its cached Project summaries for
    /// that Runner so `list_projects` sees the new Project immediately. This is
    /// a mutating Runner-side operation constrained by Runner policy; the Server
    /// never writes Project config files on the Runner host directly.
    RegisterProject {
        client_id: String,
        id: String,
        name: String,
        path: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default = "default_true")]
        allow_patch: bool,
        #[serde(default)]
        overwrite: bool,
    },

    /// Unregister one exact Runner project registration using a revision
    /// observed from `list_projects`. This removes only WebCodex registration
    /// state; it never deletes the project directory, Git worktree, or branch.
    /// The target deliberately bypasses generic project pre-resolution so a
    /// terminal `already_unregistered` Runner outcome remains representable.
    UnregisterProject {
        project: String,
        expected_revision: String,
    },

    /// Create a new directory on the selected Runner, or explicitly adopt an
    /// already-existing empty directory, and register it as a WebCodex project.
    /// The Runner validates the path against its own policy, creates or adopts
    /// the directory (and may add requested template files / git init), writes
    /// a project registration record `<project_registry_dir>/<id>.toml` atomically, and refreshes its local
    /// Project list. The Server refreshes its cached Project summaries so
    /// `list_projects` sees the new Project immediately. This is a mutating
    /// Runner-side operation constrained by Runner policy; the Server never
    /// creates directories or writes Project config files on the Runner host
    /// directly.
    CreateProject {
        client_id: String,
        id: String,
        name: String,
        path: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default = "default_true")]
        allow_patch: bool,
        #[serde(default)]
        template: Option<String>,
        #[serde(default)]
        git_init: bool,
        #[serde(default)]
        adopt_existing_empty: bool,
        #[serde(default)]
        overwrite: bool,
    },

    /// List connected Runners.
    ListRunners {
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        client_ids: Option<Vec<String>>,
        #[serde(default)]
        include_projects: Option<bool>,
        #[serde(default)]
        summary_only: bool,
    },

    /// Validate the candidate at one exact Runner process's startup-bound config
    /// path without mutating active configuration or instantiating providers.
    RunnerConfigCheck {
        client_id: String,
    },

    /// Activate the disk candidate on one exact Runner process with an optimistic
    /// active-generation fence. The Runner never accepts a filesystem path here.
    RunnerConfigReload {
        client_id: String,
        expected_generation: u64,
    },

    /// Stable gateway for Runner-owned native Tool Plugins. The static
    /// ToolDefinition is a worst-case discovery contract; execution policy is
    /// classified from `action` before scope/session/permission governance.
    PluginTool(PluginToolCall),

    /// Stable gateway for Runner-local managed SSH resources. The static
    /// ToolDefinition is a worst-case discovery contract; exact execution
    /// policy is classified from `action` before specialized governance.
    SshResource(SshResourceToolCall),

    /// Return a structured runtime health/observability summary.
    ///
    /// This is a read-only observability tool: it never exposes tokens,
    /// secrets, full env, or stdout/stderr. It returns service metadata,
    /// Project config status, Runner summaries, and Job counts.
    RuntimeStatus {
        #[serde(default)]
        compact: bool,
        #[serde(default)]
        summary_only: bool,
        #[serde(default)]
        client_id: Option<String>,
    },

    /// Admin-only bounded read of one Server-hosted full tool-request trace.
    ReadToolTrace {
        trace_ref: String,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        payload_index: Option<usize>,
    },

    /// Return a compact, bounded tool manifest with categories, risk summary,
    /// recommended flows, and optional intent-shaped tool views. Intent views
    /// only filter and rank discovery output; they do not change tool behavior,
    /// policy, permissions, execution, or finish verdict semantics. Intended as
    /// a lightweight alternative to `list_tools` for long-running tasks where
    /// full catalog schemas cause ResponseTooLargeError. Read-only runtime
    /// introspection; list/filter mode stays schema-free, while exact tool_name
    /// mode exposes only that tool's input schema. Never exposes tokens, secrets,
    /// internal paths, or output schemas.
    ToolManifest {
        /// Optional exact model-visible runtime tool name for one-tool contract discovery.
        #[serde(default)]
        tool_name: Option<String>,
        #[serde(default)]
        category: Option<String>,
        /// Optional task intent view such as coding, audit, exploration,
        /// release, or discovery. Distinct from `category`. Discovery filtering
        /// only; does not change tool behavior or finish verdict semantics.
        #[serde(default)]
        intent: Option<String>,
        #[serde(default = "default_true")]
        include_recommended_flows: bool,
        #[serde(default = "default_true")]
        include_risk_summary: bool,
    },
}

fn validate_coding_project_source_shape(tool_name: &str, arguments: &Value) -> Result<(), String> {
    let Some(arguments) = arguments.as_object() else {
        return Ok(());
    };
    let project = arguments.contains_key("project");
    let client_id = arguments.contains_key("client_id");
    let path = arguments.contains_key("path");
    for field in ["project", "client_id", "path"] {
        if arguments
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(format!(
                "invalid arguments for tool '{tool_name}': {field} must not be empty"
            ));
        }
    }
    if let Some(path) = arguments.get("path").and_then(Value::as_str) {
        if let Err(error) = validate_project_op_path(path) {
            return Err(format!("invalid arguments for tool '{tool_name}': {error}"));
        }
    }
    if project {
        let mut conflicts = Vec::new();
        if client_id {
            conflicts.push("client_id");
        }
        if path {
            conflicts.push("path");
        }
        return if conflicts.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "invalid arguments for tool '{tool_name}': conflicting fields project and {}",
                conflicts.join(", ")
            ))
        };
    }
    if path {
        return if client_id {
            Ok(())
        } else {
            Err(format!(
                "invalid arguments for tool '{tool_name}': missing client_id required with path"
            ))
        };
    }
    if client_id {
        return Err(format!(
            "invalid arguments for tool '{tool_name}': missing path required with client_id"
        ));
    }
    Err(format!(
        "invalid arguments for tool '{tool_name}': missing project source; expected project or client_id + path"
    ))
}

fn reject_unknown_read_files_fields(arguments: &Value) -> Result<(), String> {
    const ALLOWED: &[&str] = &[
        "project",
        "items",
        "session_id",
        "with_line_numbers",
        "max_result_bytes",
        // Wrapper/session metadata that transports may leave in params.
        "recording_session_id",
    ];
    let Some(object) = arguments.as_object() else {
        return Ok(());
    };
    let unknown: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|key| !ALLOWED.contains(key))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(format!(
        "invalid arguments for tool 'read_files': unknown field(s) {}",
        unknown.join(", ")
    ))
}

fn reject_unknown_observe_jobs_fields(arguments: &Value) -> Result<(), String> {
    const ALLOWED: &[&str] = &[
        "items",
        "tail_lines",
        "wait_secs",
        // Wrapper/session metadata that transports may leave in params.
        "session_id",
        "recording_session_id",
    ];
    let Some(object) = arguments.as_object() else {
        return Ok(());
    };
    let unknown: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|key| !ALLOWED.contains(key))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(format!(
        "invalid arguments for tool 'observe_jobs': unknown field(s) {}",
        unknown.join(", ")
    ))
}

fn reject_unknown_search_project_texts_fields(arguments: &Value) -> Result<(), String> {
    const ALLOWED: &[&str] = &[
        "project",
        "queries",
        "session_id",
        "max_result_bytes",
        // Wrapper/session metadata that transports may leave in params.
        "recording_session_id",
    ];
    let Some(object) = arguments.as_object() else {
        return Ok(());
    };
    let unknown: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|key| !ALLOWED.contains(key))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(format!(
        "invalid arguments for tool 'search_project_texts': unknown field(s) {}",
        unknown.join(", ")
    ))
}

fn reject_unknown_git_diff_hunks_fields(arguments: &Value) -> Result<(), String> {
    const ALLOWED: &[&str] = &[
        "project",
        "paths",
        "max_hunks",
        "max_hunk_lines",
        "max_page_bytes",
        "cached",
        "base_commit",
        "head_commit",
        "continuation",
        "session_id",
        // Wrapper/session metadata that transports may leave in params.
        "recording_session_id",
    ];
    let Some(object) = arguments.as_object() else {
        return Ok(());
    };
    let unknown = object
        .keys()
        .map(String::as_str)
        .filter(|key| !ALLOWED.contains(key))
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "invalid arguments for tool 'git_diff_hunks': unknown field(s) {}",
            unknown.join(", ")
        ))
    }
}

fn reject_unknown_targeted_inventory_fields(
    tool_name: &str,
    arguments: &Value,
) -> Result<(), String> {
    let allowed: &[&str] = match tool_name {
        "list_projects" => &["client_id", "project", "query", "limit", "summary_only"],
        "list_runners" => &[
            "client_id",
            "client_ids",
            "include_projects",
            "summary_only",
        ],
        "runtime_status" => &["client_id", "compact", "summary_only"],
        "runner_config_check" => &["client_id"],
        "runner_config_reload" => &["client_id", "expected_generation"],
        "list_jobs" => &["limit", "status", "project", "session_id"],
        _ => return Ok(()),
    };
    let Some(object) = arguments.as_object() else {
        return Ok(());
    };
    let unknown: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|key| !allowed.contains(key))
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "invalid arguments for tool '{tool_name}': unknown field(s) {}",
            unknown.join(", ")
        ))
    }
}

fn reject_unknown_bounded_computer_fields(
    tool_name: &str,
    arguments: &Value,
) -> Result<(), String> {
    let allowed: &[&str] = match tool_name {
        "computer_list_applications" | "computer_list_displays" => &["client_id", "limit"],
        "computer_launch_application" => &["client_id", "application_id"],
        "computer_snapshot_display" => &["client_id", "display_id", "max_width", "max_height"],
        "computer_read_clipboard" => &["client_id"],
        "computer_write_clipboard" => &["client_id", "text"],
        "computer_pointer_move" | "computer_pointer_click" => {
            &["client_id", "display_id", "snapshot_generation", "x", "y"]
        }
        _ => return Ok(()),
    };
    let Some(object) = arguments.as_object() else {
        return Ok(());
    };
    let unknown: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|key| !allowed.contains(key))
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "invalid arguments for tool '{tool_name}': unknown field(s) {}",
            unknown.join(", ")
        ))
    }
}

fn validate_structured_validation_sync_wait(name: &str, arguments: &Value) -> Result<(), String> {
    if !matches!(name, "cargo_fmt" | "cargo_check" | "cargo_test" | "go_test") {
        return Ok(());
    }
    let Some(object) = arguments.as_object() else {
        return Ok(());
    };
    let Some(sync_wait_value) = object.get("sync_wait_secs") else {
        return Ok(());
    };
    if sync_wait_value.is_null() {
        return Ok(());
    }
    let Some(sync_wait_secs) = sync_wait_value.as_u64() else {
        return Ok(()); // serde reports the canonical type error below.
    };
    if sync_wait_secs == 0 {
        return Err(format!(
            "invalid arguments for tool '{name}': sync_wait_secs must be at least 1"
        ));
    }
    if name == "cargo_fmt" && object.get("check").and_then(Value::as_bool) != Some(true) {
        return Err(
            "invalid arguments for tool 'cargo_fmt': sync_wait_secs is available only with check=true"
                .to_string(),
        );
    }
    Ok(())
}

impl ToolCall {
    pub fn from_tool_name(name: &str, arguments: Value) -> Result<Self, String> {
        Self::from_tool_name_with_recorder_metadata(name, arguments).map(|(call, _)| call)
    }

    /// Parse a model/runtime tool call while returning recorder-only expectation
    /// metadata separately. The metadata is stripped before ToolCall serde so it
    /// can never become part of the concrete execution input.
    pub fn from_tool_name_with_recorder_metadata(
        name: &str,
        arguments: Value,
    ) -> Result<(Self, ToolCallRecorderMetadata), String> {
        if name == "create_project"
            && arguments
                .as_object()
                .is_some_and(|object| object.contains_key("managed_temporary_project"))
        {
            return Err(
                "invalid arguments for tool 'create_project': field 'managed_temporary_project' is no longer supported; use ordinary explicit project creation"
                    .to_string(),
            );
        }
        if name == "create_project"
            && arguments
                .as_object()
                .is_some_and(|object| object.contains_key("allow_existing_empty"))
        {
            return Err(
                "invalid arguments for tool 'create_project': field 'allow_existing_empty' is no longer supported; use 'adopt_existing_empty' to explicitly adopt an existing empty directory"
                    .to_string(),
            );
        }
        // Reject unknown tool names up front with a helpful message that lists
        // every accepted tool and points the caller at listRuntimeTools. This
        // avoids leaking a raw serde "unknown variant" error and gives custom
        // GPTs an actionable discovery hint.
        let definition = lookup_tool_definition(name).ok_or_else(|| {
            format!(
                "unknown tool '{}'. Available tools: {}. Call listRuntimeTools \
                 (POST /api/tools/list) or the list_tools runtime tool to \
                 discover accepted tool names.",
                name,
                model_visible_tool_names_csv()
            )
        })?;
        validate_model_facing_assertion_name(name, &arguments)?;
        validate_model_facing_result_expectation(name, &arguments)?;
        validate_structured_validation_sync_wait(name, &arguments)?;
        if name == "apply_patch"
            && arguments
                .as_object()
                .is_some_and(|object| object.contains_key("strict_matching"))
        {
            return Err(
                "invalid arguments for tool 'apply_patch': field 'strict_matching' is no longer supported; use matching_mode='exact_unique' for former strict_matching=true, matching_mode='first_match' for former strict_matching=false, or omit matching_mode for the current unique default"
                    .to_string(),
            );
        }
        let recorder_metadata = ToolCallRecorderMetadata::from_business_arguments(&arguments);
        let arguments = strip_tool_call_expectation_metadata(arguments);
        if name == "read_project_artifact"
            && arguments
                .as_object()
                .is_some_and(|object| object.contains_key("max_bytes"))
        {
            return Err(
                "invalid arguments for tool 'read_project_artifact': field 'max_bytes' is no longer supported; use 'length'"
                    .to_string(),
            );
        }
        if name == "write_project_file"
            && arguments
                .as_object()
                .is_some_and(|object| object.contains_key("expected_content_prefix"))
        {
            return Err(
                "invalid arguments for tool 'write_project_file': field 'expected_content_prefix' is no longer supported; use expected_sha256"
                    .to_string(),
            );
        }
        if name == "work_on_project" {
            validate_coding_project_source_shape(name, &arguments)?;
        }
        if name == "read_files" {
            reject_unknown_read_files_fields(&arguments)?;
        }
        if name == "observe_jobs" {
            reject_unknown_observe_jobs_fields(&arguments)?;
        }
        if name == "search_project_texts" {
            reject_unknown_search_project_texts_fields(&arguments)?;
        }
        if name == "git_diff_hunks" {
            reject_unknown_git_diff_hunks_fields(&arguments)?;
        }
        if matches!(
            name,
            "list_projects"
                | "list_runners"
                | "runtime_status"
                | "list_jobs"
                | "runner_config_check"
                | "runner_config_reload"
        ) {
            reject_unknown_targeted_inventory_fields(name, &arguments)?;
        }
        if matches!(
            name,
            "computer_list_applications"
                | "computer_launch_application"
                | "computer_list_displays"
                | "computer_snapshot_display"
                | "computer_read_clipboard"
                | "computer_write_clipboard"
                | "computer_pointer_move"
                | "computer_pointer_click"
        ) {
            reject_unknown_bounded_computer_fields(name, &arguments)?;
        }
        let mut wrapped = serde_json::Map::new();
        wrapped.insert(
            TOOL_CALL_TOOL_FIELD.to_string(),
            Value::String(name.to_string()),
        );
        if definition.requires_artifact_upload_path_binding() {
            let missing_path = arguments
                .as_object()
                .and_then(|obj| obj.get("path"))
                .and_then(Value::as_str)
                .map(str::is_empty)
                .unwrap_or(true);
            if missing_path {
                return Err(format!(
                    "invalid arguments for tool '{}': path is required and must match the path \
                     used by artifact_upload_begin to bind upload_id to the requested target path",
                    name
                ));
            }
        }
        if !definition.uses_unit_arguments() {
            // Non-unit tools always carry a `params` object so variants whose
            // fields are all optional (e.g. `list_jobs`) still deserialize when
            // a caller passes `null` arguments. A null argument is normalized
            // to an empty object; required-field validation still fires for
            // tools that need fields.
            let params = if arguments.is_null() {
                Value::Object(serde_json::Map::new())
            } else {
                arguments
            };
            wrapped.insert(TOOL_CALL_PARAMS_FIELD.to_string(), params);
        }
        let call: Self = serde_json::from_value(Value::Object(wrapped))
            .map_err(|e| format!("invalid arguments for tool '{}': {}", name, e))?;
        if let Self::PluginTool(plugin) = &call {
            plugin
                .validate()
                .map_err(|error| format!("invalid arguments for tool '{}': {}", name, error))?;
        }
        if let Self::SshResource(ssh_resource) = &call {
            ssh_resource
                .validate()
                .map_err(|error| format!("invalid arguments for tool '{}': {}", name, error))?;
        }
        Ok((call, recorder_metadata))
    }

    /// Raw command text for shell-like calls. Consumed only by the workspace
    /// activity recorder, which truncates it to a bounded preview and honors
    /// the operator's preview config switch — it is never logged verbatim.
    pub fn command_text(&self) -> Option<&str> {
        match self {
            Self::RunShell { command, .. }
            | Self::SessionShellExec { command, .. }
            | Self::RunJob { command, .. } => Some(command),
            _ => None,
        }
    }

    pub fn tool_name(&self) -> &'static str {
        match self {
            Self::ListTools { .. } => "list_tools",
            Self::StartSession { .. } => "start_session",
            Self::WorkOnProject { .. } => "work_on_project",
            Self::FinishCodingTask { .. } => "finish_coding_task",
            Self::PresentWorkResult { .. } => "present_work_result",
            Self::WorkResultState { .. } => "work_result_state",
            Self::SessionSummary { .. } => "session_summary",
            Self::UpdateSessionContext { .. } => "update_session_context",
            Self::CloseSession { .. } => "close_session",
            Self::ValidationSummary { .. } => "validation_summary",
            Self::PostSessionMessage { .. } => "post_session_message",
            Self::ListSessionMessages { .. } => "list_session_messages",
            Self::GetSessionAssignment { .. } => "get_session_assignment",
            Self::ObserveSessionMessages { .. } => "observe_session_messages",
            Self::ResolveSessionMessage { .. } => "resolve_session_message",
            Self::CompleteSessionMessage { .. } => "complete_session_message",
            Self::SessionDiscussionSummary { .. } => "session_discussion_summary",
            Self::SessionHandoffSummary { .. } => "session_handoff_summary",
            #[cfg(feature = "workspace-checkpoints")]
            Self::WorkspaceCheckpointCreate { .. } => "workspace_checkpoint_create",
            #[cfg(feature = "workspace-checkpoints")]
            Self::WorkspaceCheckpointList { .. } => "workspace_checkpoint_list",
            #[cfg(feature = "workspace-checkpoints")]
            Self::WorkspaceCheckpointShow { .. } => "workspace_checkpoint_show",
            #[cfg(feature = "workspace-checkpoints")]
            Self::WorkspaceCheckpointRestore { .. } => "workspace_checkpoint_restore",
            #[cfg(feature = "workspace-checkpoints")]
            Self::WorkspaceCheckpointDelete { .. } => "workspace_checkpoint_delete",
            Self::RunProcess { .. } => "run_process",
            Self::RunDetachedProcess { .. } => "run_detached_process",
            Self::CodingAgentStart { .. } => "coding_agent_start",
            Self::CodingAgentObserve { .. } => "coding_agent_observe",
            Self::CodingAgentCancel { .. } => "coding_agent_cancel",
            Self::RunScript { .. } => "run_script",
            Self::RunShell { .. } => "run_shell",
            Self::OpenSessionShell { .. } => "open_session_shell",
            Self::SessionShellExec { .. } => "session_shell_exec",
            Self::SessionShellStatus { .. } => "session_shell_status",
            Self::CloseSessionShell { .. } => "close_session_shell",
            Self::ApplyPatch { .. } => "apply_patch",
            Self::ApplyUnifiedDiff { .. } => "apply_unified_diff",
            Self::DeleteProjectFiles { .. } => "delete_project_files",
            Self::GitRestorePaths { .. } => "git_restore_paths",
            Self::DiscardUntracked { .. } => "discard_untracked",
            Self::GitCommitPaths { .. } => "git_commit_paths",
            Self::GitStatus { .. } => "git_status",
            Self::GitDiffHunks { .. } => "git_diff_hunks",
            Self::GitReviewSummary { .. } => "git_review_summary",
            Self::GitLog { .. } => "git_log",
            Self::CargoFmt { .. } => "cargo_fmt",
            Self::CargoCheck { .. } => "cargo_check",
            Self::CargoTest { .. } => "cargo_test",
            Self::GoTest { .. } => "go_test",
            Self::ReadFiles { .. } => "read_files",
            Self::SkillList { .. } => "skill_list",
            Self::SkillReadFile { .. } => "skill_read_file",
            Self::SkillVersions { .. } => "skill_versions",
            Self::SkillInstall { .. } => "skill_install",
            Self::SkillActivate { .. } => "skill_activate",
            Self::SkillRemoveRevision { .. } => "skill_remove_revision",
            Self::CreateGoal { .. } => "create_goal",
            Self::GetGoal { .. } => "get_goal",
            Self::PresentGoalPlan { .. } => "present_goal_plan",
            Self::GoalPlanState { .. } => "goal_plan_state",
            Self::ListGoals { .. } => "list_goals",
            Self::UpdateGoal { .. } => "update_goal",
            Self::AssociateGoalAgentTask { .. } => "associate_goal_agent_task",
            Self::AssociateGoalWorkflowSession { .. } => "associate_goal_workflow_session",
            Self::CreateAgentTask { .. } => "create_agent_task",
            Self::ListAgentTasks { .. } => "list_agent_tasks",
            Self::ReadAgentTask { .. } => "read_agent_task",
            Self::AssignAgentTask { .. } => "assign_agent_task",
            Self::StartAgentTaskAttempt { .. } => "start_agent_task_attempt",
            Self::StartAgentTaskEndpointContinuation { .. } => {
                "start_agent_task_endpoint_continuation"
            }
            Self::StartAgentTaskCodingRun { .. } => "start_agent_task_coding_run",
            Self::ReconcileAgentTaskCodingRun { .. } => "reconcile_agent_task_coding_run",
            Self::HeartbeatAgentTaskAttempt { .. } => "heartbeat_agent_task_attempt",
            Self::CompleteAgentTaskAttempt { .. } => "complete_agent_task_attempt",
            Self::CreateAgentIdentity { .. } => "create_agent_identity",
            Self::ListAgentIdentities { .. } => "list_agent_identities",
            Self::UpdateAgentIdentity { .. } => "update_agent_identity",
            Self::RotateAgentContinuationEndpoint { .. } => "rotate_agent_continuation_endpoint",
            Self::AttachAgentEndpoint { .. } => "attach_agent_endpoint",
            Self::PresentAgentContinuation { .. } => "present_agent_continuation",
            Self::AgentContinuationBind { .. } => "agent_continuation_bind",
            Self::AgentContinuationRecoverEndpoint { .. } => "agent_continuation_recover_endpoint",
            Self::AgentContinuationState { .. } => "agent_continuation_state",
            Self::AgentContinuationWakeAcquire { .. } => "agent_continuation_wake_acquire",
            Self::AgentContinuationWakePrepare { .. } => "agent_continuation_wake_prepare",
            Self::AgentContinuationWakeFinish { .. } => "agent_continuation_wake_finish",
            Self::AgentContinuationUnbind { .. } => "agent_continuation_unbind",
            Self::DetachAgentEndpoint { .. } => "detach_agent_endpoint",
            Self::CreateConversation { .. } => "create_conversation",
            Self::ListConversations { .. } => "list_conversations",
            Self::ReadConversation { .. } => "read_conversation",
            Self::PostConversationMessage { .. } => "post_conversation_message",
            Self::ListAgentInbox { .. } => "list_agent_inbox",
            Self::ConsumeAgentDeliveries { .. } => "consume_agent_deliveries",
            Self::BootstrapAgentConversation { .. } => "bootstrap_agent_conversation",
            Self::ConsumeAgentWake { .. } => "consume_agent_wake",
            Self::MemorySearch { .. } => "memory_search",
            Self::MemoryRead { .. } => "memory_read",
            Self::MemorySet { .. } => "memory_set",
            Self::MemoryDelete { .. } => "memory_delete",
            Self::MemoryScopeList { .. } => "memory_scope_list",
            Self::MemoryScopePurge { .. } => "memory_scope_purge",
            Self::RunJob { .. } => "run_job",
            Self::StopJob { .. } => "stop_job",
            Self::ObserveJobs { .. } => "observe_jobs",
            Self::ListProjectFiles { .. } => "list_project_files",
            Self::ListProjectTrackedFiles { .. } => "list_project_tracked_files",
            Self::ProjectOverview { .. } => "project_overview",
            Self::SearchProjectTexts { .. } => "search_project_texts",
            Self::ShowChanges { .. } => "show_changes",
            Self::WorkspaceHygieneCheck { .. } => "workspace_hygiene_check",
            Self::ListJobs { .. } => "list_jobs",
            Self::JobTail { .. } => "job_tail",
            Self::WriteProjectFile { .. } => "write_project_file",
            Self::SaveProjectArtifact { .. } => "save_project_artifact",
            Self::ImportConversationFilesToProject { .. } => "import_conversation_files_to_project",
            Self::ExportProjectArtifact { .. } => "export_project_artifact",
            Self::ReadProjectArtifactMetadata { .. } => "read_project_artifact_metadata",
            Self::ReadProjectArtifact { .. } => "read_project_artifact",
            Self::ArtifactUploadBegin { .. } => "artifact_upload_begin",
            Self::ArtifactUploadChunk { .. } => "artifact_upload_chunk",
            Self::ArtifactUploadFinish { .. } => "artifact_upload_finish",
            Self::ArtifactUploadAbort { .. } => "artifact_upload_abort",
            Self::ApplyTextEdits { .. } => "apply_text_edits",
            Self::LspStatus { .. } => "lsp_status",
            Self::DocumentSymbols { .. } => "document_symbols",
            Self::DocumentDiagnostics { .. } => "document_diagnostics",
            Self::Hover { .. } => "hover",
            Self::WorkspaceSymbols { .. } => "workspace_symbols",
            Self::GotoDefinition { .. } => "goto_definition",
            Self::FindReferences { .. } => "find_references",
            Self::CallHierarchy { .. } => "call_hierarchy",
            Self::ComputerListTargets => "computer_list_targets",
            Self::ComputerListWindows { .. } => "computer_list_windows",
            Self::ComputerListApplications { .. } => "computer_list_applications",
            Self::ComputerListDisplays { .. } => "computer_list_displays",
            Self::ComputerLaunchApplication { .. } => "computer_launch_application",
            Self::ComputerAccessibilityStatus { .. } => "computer_accessibility_status",
            Self::ComputerAccessibilityTree { .. } => "computer_accessibility_tree",
            Self::ComputerFindElements { .. } => "computer_find_elements",
            Self::ComputerElementState { .. } => "computer_element_state",
            Self::ComputerActivateWindow { .. } => "computer_activate_window",
            Self::ComputerControl { .. } => "computer_control",
            Self::ComputerScrollToElement { .. } => "computer_scroll_to_element",
            Self::ComputerKeyInput { .. } => "computer_key_input",
            Self::ComputerReadClipboard { .. } => "computer_read_clipboard",
            Self::ComputerWriteClipboard { .. } => "computer_write_clipboard",
            Self::ComputerPointerMove { .. } => "computer_pointer_move",
            Self::ComputerPointerClick { .. } => "computer_pointer_click",
            Self::ComputerInputText { .. } => "computer_input_text",
            Self::ComputerSnapshot { .. } => "computer_snapshot",
            Self::ComputerSnapshotDisplay { .. } => "computer_snapshot_display",
            Self::ComputerSaveSnapshot { .. } => "computer_save_snapshot",
            Self::ListProjects { .. } => "list_projects",
            Self::RegisterProject { .. } => "register_project",
            Self::UnregisterProject { .. } => "unregister_project",
            Self::CreateProject { .. } => "create_project",
            Self::ListRunners { .. } => "list_runners",
            Self::RunnerConfigCheck { .. } => "runner_config_check",
            Self::RunnerConfigReload { .. } => "runner_config_reload",
            Self::PluginTool(_) => "plugin_tool",
            Self::SshResource(_) => "ssh_resource",
            Self::RuntimeStatus { .. } => "runtime_status",
            Self::ReadToolTrace { .. } => "read_tool_trace",
            Self::ToolManifest { .. } => "tool_manifest",
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::RunProcess { session_id, .. }
            | Self::RunDetachedProcess { session_id, .. }
            | Self::RunScript { session_id, .. }
            | Self::RunShell { session_id, .. }
            | Self::ApplyPatch { session_id, .. }
            | Self::ApplyUnifiedDiff { session_id, .. }
            | Self::DeleteProjectFiles { session_id, .. }
            | Self::GitRestorePaths { session_id, .. }
            | Self::DiscardUntracked { session_id, .. }
            | Self::GitCommitPaths { session_id, .. }
            | Self::GitStatus { session_id, .. }
            | Self::GitDiffHunks { session_id, .. }
            | Self::GitReviewSummary { session_id, .. }
            | Self::GitLog { session_id, .. }
            | Self::CargoFmt { session_id, .. }
            | Self::CargoCheck { session_id, .. }
            | Self::CargoTest { session_id, .. }
            | Self::GoTest { session_id, .. }
            | Self::ReadFiles { session_id, .. }
            | Self::SkillList { session_id, .. }
            | Self::SkillReadFile { session_id, .. }
            | Self::SkillVersions { session_id, .. }
            | Self::SkillInstall { session_id, .. }
            | Self::SkillActivate { session_id, .. }
            | Self::SkillRemoveRevision { session_id, .. }
            | Self::MemorySearch { session_id, .. }
            | Self::MemoryRead { session_id, .. }
            | Self::MemorySet { session_id, .. }
            | Self::MemoryDelete { session_id, .. }
            | Self::RunJob { session_id, .. }
            | Self::StopJob { session_id, .. }
            | Self::ListProjectFiles { session_id, .. }
            | Self::ListProjectTrackedFiles { session_id, .. }
            | Self::ProjectOverview { session_id, .. }
            | Self::SearchProjectTexts { session_id, .. }
            | Self::ShowChanges { session_id, .. }
            | Self::WriteProjectFile { session_id, .. }
            | Self::SaveProjectArtifact { session_id, .. }
            | Self::ComputerSaveSnapshot { session_id, .. }
            | Self::ExportProjectArtifact { session_id, .. }
            | Self::ReadProjectArtifactMetadata { session_id, .. }
            | Self::ReadProjectArtifact { session_id, .. }
            | Self::ArtifactUploadBegin { session_id, .. }
            | Self::ArtifactUploadChunk { session_id, .. }
            | Self::ArtifactUploadFinish { session_id, .. }
            | Self::ArtifactUploadAbort { session_id, .. }
            | Self::ApplyTextEdits { session_id, .. }
            | Self::WorkspaceHygieneCheck { session_id, .. }
            | Self::LspStatus { session_id, .. }
            | Self::DocumentSymbols { session_id, .. }
            | Self::DocumentDiagnostics { session_id, .. }
            | Self::Hover { session_id, .. }
            | Self::WorkspaceSymbols { session_id, .. }
            | Self::GotoDefinition { session_id, .. }
            | Self::FindReferences { session_id, .. } => session_id.as_deref(),
            #[cfg(feature = "workspace-checkpoints")]
            Self::WorkspaceCheckpointCreate { session_id, .. }
            | Self::WorkspaceCheckpointList { session_id, .. }
            | Self::WorkspaceCheckpointShow { session_id, .. }
            | Self::WorkspaceCheckpointRestore { session_id, .. }
            | Self::WorkspaceCheckpointDelete { session_id, .. } => session_id.as_deref(),
            Self::SessionHandoffSummary { session_id, .. } => Some(session_id.as_str()),
            Self::PresentWorkResult { session_id, .. } => Some(session_id.as_str()),
            // work_result_state intentionally does not expose its business
            // Session through this generic recorder projection: App polling
            // authorizes and reads that exact target inside its runtime method.
            Self::WorkResultState { .. } => None,
            Self::ImportConversationFilesToProject { session_id, .. } => session_id.as_deref(),
            Self::CallHierarchy { session_id, .. } => session_id.as_deref(),
            Self::WorkOnProject { session_id, .. } => session_id.as_deref(),
            Self::OpenSessionShell { session_id, .. }
            | Self::SessionShellExec { session_id, .. }
            | Self::SessionShellStatus { session_id, .. }
            | Self::CloseSessionShell { session_id, .. } => Some(session_id.as_str()),
            _ => None,
        }
    }

    /// Attach explicit generic recorder provenance to a CodingAgentStart only.
    /// This never makes the recorder a business Session or Run authority.
    pub fn with_coding_agent_recording_session_id(
        mut self,
        recorder_session_id: Option<String>,
    ) -> Self {
        if let (
            Self::CodingAgentStart {
                recording_session_id,
                ..
            },
            Some(recorder_session_id),
        ) = (&mut self, recorder_session_id)
        {
            if recording_session_id.is_none() {
                *recording_session_id = Some(recorder_session_id);
            }
        }
        self
    }

    /// Apply project-matched Session defaults without overwriting explicit
    /// per-call arguments.
    pub fn with_session_execution_context(
        mut self,
        execution_context: &SessionExecutionContext,
    ) -> Self {
        match &mut self {
            Self::RunProcess { cwd, .. }
            | Self::RunDetachedProcess { cwd, .. }
            | Self::RunScript { cwd, .. }
                if cwd.is_none() =>
            {
                *cwd = execution_context.default_cwd.clone();
            }
            Self::RunShell { cwd, shell, .. }
            | Self::RunJob { cwd, shell, .. }
            | Self::OpenSessionShell { cwd, shell, .. } => {
                if cwd.is_none() {
                    *cwd = execution_context.default_cwd.clone();
                }
                if shell.is_none() {
                    *shell = execution_context.default_shell;
                }
            }
            _ => {}
        }
        self
    }

    pub fn project(&self) -> Option<&str> {
        match self {
            Self::RunProcess { project, .. }
            | Self::RunDetachedProcess { project, .. }
            | Self::CodingAgentStart { project, .. }
            | Self::StartAgentTaskCodingRun { project, .. }
            | Self::RunScript { project, .. }
            | Self::RunShell { project, .. }
            | Self::OpenSessionShell { project, .. }
            | Self::SessionShellExec { project, .. }
            | Self::SessionShellStatus { project, .. }
            | Self::CloseSessionShell { project, .. }
            | Self::ApplyPatch { project, .. }
            | Self::ApplyUnifiedDiff { project, .. }
            | Self::DeleteProjectFiles { project, .. }
            | Self::GitRestorePaths { project, .. }
            | Self::DiscardUntracked { project, .. }
            | Self::GitCommitPaths { project, .. }
            | Self::GitStatus { project, .. }
            | Self::GitDiffHunks { project, .. }
            | Self::GitReviewSummary { project, .. }
            | Self::GitLog { project, .. }
            | Self::CargoFmt { project, .. }
            | Self::CargoCheck { project, .. }
            | Self::CargoTest { project, .. }
            | Self::GoTest { project, .. }
            | Self::ReadFiles { project, .. }
            | Self::SkillList { project, .. }
            | Self::SkillReadFile { project, .. }
            | Self::SkillVersions { project, .. }
            | Self::SkillInstall { project, .. }
            | Self::SkillActivate { project, .. }
            | Self::SkillRemoveRevision { project, .. }
            | Self::MemorySearch { project, .. }
            | Self::MemoryRead { project, .. }
            | Self::MemorySet { project, .. }
            | Self::MemoryDelete { project, .. }
            | Self::RunJob { project, .. }
            | Self::StopJob { project, .. }
            | Self::ListProjectFiles { project, .. }
            | Self::ListProjectTrackedFiles { project, .. }
            | Self::ProjectOverview { project, .. }
            | Self::SearchProjectTexts { project, .. }
            | Self::ShowChanges { project, .. }
            | Self::WriteProjectFile { project, .. }
            | Self::SaveProjectArtifact { project, .. }
            | Self::ComputerSaveSnapshot { project, .. }
            | Self::ImportConversationFilesToProject { project, .. }
            | Self::ExportProjectArtifact { project, .. }
            | Self::ReadProjectArtifactMetadata { project, .. }
            | Self::ReadProjectArtifact { project, .. }
            | Self::ArtifactUploadBegin { project, .. }
            | Self::ArtifactUploadChunk { project, .. }
            | Self::ArtifactUploadFinish { project, .. }
            | Self::ArtifactUploadAbort { project, .. }
            | Self::ApplyTextEdits { project, .. }
            | Self::WorkspaceHygieneCheck { project, .. }
            | Self::LspStatus { project, .. }
            | Self::DocumentSymbols { project, .. }
            | Self::DocumentDiagnostics { project, .. }
            | Self::Hover { project, .. }
            | Self::WorkspaceSymbols { project, .. }
            | Self::GotoDefinition { project, .. }
            | Self::FindReferences { project, .. } => Some(project.as_str()),
            #[cfg(feature = "workspace-checkpoints")]
            Self::WorkspaceCheckpointCreate { project, .. }
            | Self::WorkspaceCheckpointList { project, .. }
            | Self::WorkspaceCheckpointShow { project, .. }
            | Self::WorkspaceCheckpointRestore { project, .. }
            | Self::WorkspaceCheckpointDelete { project, .. } => Some(project.as_str()),
            Self::CallHierarchy { project, .. } => Some(project.as_str()),
            Self::WorkOnProject { project, .. } if !project.trim().is_empty() => {
                Some(project.as_str())
            }
            Self::FinishCodingTask { project, .. }
            | Self::PresentWorkResult { project, .. }
            | Self::WorkResultState { project, .. } => Some(project.as_str()),
            Self::UpdateSessionContext { project, .. }
            | Self::ValidationSummary { project, .. } => Some(project.as_str()),
            Self::SessionHandoffSummary { project, .. } => project.as_deref(),
            _ => None,
        }
    }
}
