use base64::{engine::general_purpose, Engine as _};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use tokio::time::Instant;
use webcodex_workspace::file_read_range::{self, EffectiveRange, FileReadRange, ReadFileReason};

#[cfg(test)]
use super::helpers::run_command_sync;
use super::helpers::{
    bounded_tail, looks_like_command_timeout, shell_escape_simple, validate_limited_cleanup_paths,
    validate_project_relative_path,
};
use super::project_resolution::ResolvedProject;
use super::shell::{dispatch_uncertainty_lifecycle, runner_command_lifecycle};
use super::tool_inputs::{
    ApplyFileChangeInput, ApplyFileChangeKind, ApplyTextEditInput, ApplyTextEditKind,
};
use super::tool_result::ToolResult;
use super::{file_listing, permissions, project_instructions};
use super::{SearchPatternMode, SearchResultMode, ToolRuntime};
use crate::artifact_policy::{
    has_safe_octet_stream_artifact_extension, octet_stream_safe_extension_error,
    ooxml_extension_for_mime, MAX_MCP_IMAGE_BYTES,
};
use crate::auth::AuthContext;
use crate::project_overview::{
    effective_project_overview_limit, effective_project_overview_max_depth,
    normalize_project_overview_path,
};
use crate::projects::ProjectConfig;
use crate::runner_protocol::{
    ShellCommandExecutionState, ShellFileOpRequest, ShellRunRequest, ShellRunResponse,
    EXTERNAL_SEARCH_REQUEST_PREFIX,
};

mod artifacts;
mod inspection;
mod mutations;
mod search;

pub(crate) use artifacts::{
    validate_artifact_file_path, validate_artifact_mime_for_path,
    validate_project_artifact_export_snapshot, ProjectArtifactExportSnapshot,
    MAX_PROJECT_ARTIFACT_EXPORT_BYTES, MAX_PROJECT_ARTIFACT_UPLOAD_CHUNK_BYTES,
    MAX_READ_PROJECT_ARTIFACT_LENGTH,
};
#[cfg(test)]
pub(crate) use artifacts::{MAX_PROJECT_ARTIFACT_BYTES, MAX_PROJECT_ARTIFACT_UPLOAD_BYTES};
#[cfg(test)]
pub(crate) use inspection::{
    page_file_list_entries, parse_file_list_entries, LIST_TRACKED_STDERR_MAX_CHARS,
};
#[cfg(test)]
pub(crate) use mutations::{apply_text_edits_to_string, validate_edit_file_path};
use search::search_head_resolution_shell;
#[cfg(test)]
pub(crate) use search::{
    resolve_search_head_command, search_agent_timeout_budget, search_project_text_command,
    search_project_text_command_with_head_fallbacks, search_project_text_output,
    MAX_SEARCH_CONTEXT_LINES, MAX_SEARCH_GLOBS, MAX_SEARCH_GLOB_BYTES, SEARCH_OUTPUT_BYTE_BUDGET,
};
pub(crate) use search::{
    SearchOptions, SearchRequest, DEFAULT_SEARCH_HEAD_ABSOLUTE_CANDIDATES,
    DEFAULT_SEARCH_TIMEOUT_SECS,
};

// Edit limits and the sensitive-path guard are shared with the Runner binary.
#[cfg(test)]
pub(crate) use crate::apply_edits_shared::{
    canonicalize_apply_text_line_endings, detect_apply_text_line_ending,
    restore_apply_text_line_endings,
};
pub(crate) use crate::apply_edits_shared::{
    is_lowercase_hex_sha256 as is_hex_sha256, is_sensitive_edit_path, MAX_APPLY_FILE_CHANGES,
    MAX_APPLY_TEXT_EDITS, MAX_APPLY_TEXT_EDIT_FIELD_BYTES,
};

pub(crate) fn sha256_hex_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

impl ToolRuntime {
    // Phase 4: native Runner JSON file ops
    // -------------------------------------------------------------------------
    //
    // Structured edits and project artifact tools run through the owning Runner.
    // The server never reads or writes the Runner project filesystem directly.
    // Arguments travel as JSON in a native Runner file-op payload; the agent
    // performs validation and returns one JSON object on stdout.

    pub(crate) async fn run_runner_json_file_op(
        &self,
        client_id: String,
        cwd: String,
        path: String,
        op: &str,
        payload: Value,
        tool_name: &str,
    ) -> Result<Value, String> {
        let serialized = serde_json::to_string(&payload)
            .map_err(|e| format!("failed to serialize file-op payload: {}", e))?;
        let wait_timeout = 60_u64;
        let (request_id, rx) = self
            .runner_registry
            .enqueue_file_op(
                ShellFileOpRequest {
                    op: op.to_string(),
                    client_id,
                    path: path.clone(),
                    cwd: Some(cwd),
                    content: Some(serialized),
                    max_bytes: None,
                    old_text: None,
                    pattern: None,
                    expected_sha256: None,
                    expected_prefix: None,
                    start_line: None,
                    end_line: None,
                    line: None,
                    create_dirs: false,
                    wait_timeout_secs: wait_timeout,
                },
                "tool_runtime".to_string(),
            )
            .await?;
        let resp = match tokio::time::timeout(Duration::from_secs(wait_timeout + 4), rx).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(_)) => {
                self.runner_registry.cancel_request(&request_id).await;
                return Err(format!("Runner {} request was dropped", tool_name));
            }
            Err(_) => {
                self.runner_registry.cancel_request(&request_id).await;
                return Err(format!("timed out waiting for Runner {}", tool_name));
            }
        };
        if let Some(e) = resp.error {
            return Err(e);
        }
        if resp.exit_code != Some(0) {
            return Err(resp.stderr.unwrap_or_else(|| {
                format!("Runner {} failed with code {:?}", tool_name, resp.exit_code)
            }));
        }
        let stdout = resp.stdout.unwrap_or_default();
        let stdout = stdout.trim();
        serde_json::from_str(stdout).map_err(|e| {
            format!(
                "Runner {} returned invalid JSON: {} (got: {})",
                tool_name,
                e,
                &stdout[..stdout.len().min(200)]
            )
        })
    }
}
