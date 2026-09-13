//! Shared model-facing projection for canonical coding workflow startup.
//!
//! The runtime builds this once and every transport carries the same core
//! value. The projection is deterministic, bounded, path-safe, and contains
//! only the facts a coding model needs to start or continue work.

use serde::Serialize;
use serde_json::{json, Value};
#[cfg(test)]
use std::collections::BTreeSet;

use super::continuation_feedback::EXPLORATION_CONTINUITY_ACTION;
use super::project_instructions::{
    ProjectInstructionFile, ProjectInstructionsSnapshot, ProjectInstructionsSummarySnapshot,
    INSTRUCTION_CANDIDATE_PATHS, MAX_LINES_PER_FILE,
};
use super::project_resolution::ResolvedProject;
use super::session_context::canonical_repository_key;
use super::sessions::SessionSummary;
use super::tool_inputs::StartupDetail;

// Reserve transport-envelope headroom so a ToolResult and the GPT Actions
// wrapper also remain below the externally documented 32 KiB ceiling.
pub(crate) const STANDARD_STARTUP_HARD_MAX_BYTES: usize = 30 * 1024;
pub(crate) const STARTUP_EXTENSION_CATALOG_HARD_MAX_BYTES: usize = 6 * 1024;
pub(crate) const STARTUP_SKILL_CATALOG_MAX_BYTES: usize = 2_900;
pub(crate) const STARTUP_PLUGIN_CATALOG_MAX_BYTES: usize = 2_900;
pub(crate) const STARTUP_EXTENSION_DESCRIPTION_MAX_BYTES: usize = 512;
pub(crate) const REPOSITORY_OVERVIEW_NOT_REQUESTED_REASON: &str =
    "not_requested_by_work_on_project";
const INSTRUCTION_CONTENT_JSON_BUDGET: usize = 10 * 1024;
const MIN_INSTRUCTION_CONTENT_JSON_BYTES: usize = 512;
const MAX_RULE_HEADINGS: usize = 6;
const MAX_RULE_HEADING_JSON_BYTES: usize = 160;
const MAX_CHANGED_PATHS: usize = 20;
const MAX_MINIMAL_EXPLORATION_PATHS: usize = 3;
const MAX_STANDARD_EXPLORATION_PATHS: usize = 12;
const MAX_FAILURES: usize = 10;
const MAX_SUGGESTED_ACTIONS: usize = 5;
const MAX_PATH_JSON_BYTES: usize = 192;
const MAX_FAILURE_NAME_JSON_BYTES: usize = 160;
const MAX_FAILURE_FILE_JSON_BYTES: usize = 160;
const MAX_ACTION_JSON_BYTES: usize = 384;
const MAX_INSTRUCTION_EXCERPT_JSON_BYTES: usize = 768;

#[cfg(test)]
pub(crate) use webcodex_core::runtime_contract::BUILTIN_CODING_WORKFLOW_MAX_GUIDANCE_ITEMS;
pub(crate) use webcodex_core::runtime_contract::{
    BUILTIN_CODING_WORKFLOW_CONTRACT, BUILTIN_CODING_WORKFLOW_VERSION,
};

/// Stable model-facing coding/review semantics owned by WebCodex itself.
///
/// This is intentionally not a project instruction source and is never stored
/// as Session mode, capability, permission, or execution authority. Task text
/// may name one of these roles; the projection only gives that phrase a stable
/// meaning for the model conducting the pass.
pub(crate) fn builtin_coding_workflow_projection() -> Value {
    json!({
        "contract": BUILTIN_CODING_WORKFLOW_CONTRACT,
        "version": BUILTIN_CODING_WORKFLOW_VERSION,
        "authority": "model_guidance_only",
        "role_selection": "Default guidance always applies; named roles only when requested. Neither grants authority.",
        "guidance": [
            "Defaults only: follow host safety, user scope, and project rules; guidance grants no authority.",
            "Complete authorized work through validation and review; ask only for missing requirements or authority.",
            "Verify Project, branch, HEAD, and existing changes; read nested rules for changed paths and recover truncated instructions.",
            "Preserve unrelated work; make the smallest coherent change. Push/publish/deploy/restart need an explicit action and target.",
            "Use structured tools and edit guards; apply model_protocol only where the exposed schema supports it.",
            "Long required validation + independent read-only inspection: use short sync_wait_secs for same-execution Job handoff, inspect then observe; do not fan out heavy validations. Covered-source mutation makes that result stale/cache-warmup; final source needs fresh validation.",
            "Observe existing Jobs; inspect state before retrying an unknown outcome. Timeout does not prove no effect.",
            "Review the diff; report evidence, limits, and Jobs. finish_coding_task is advisory evidence, not proof."
        ],
        "model_protocol": {
            "session_context_ack": "Checkpoint/recovery tools may expose session_context_revision. Echo the latest retained revision in ack_session_context_revision only where exposed; never invent it. If unknown, omit; use the advertised Session handoff recovery path. ACK is nonblocking.",
            "session_recording": "When work_on_project creates or resumes, pass recording_session_id for recorder provenance only. business session_id may target another Session; it grants no authority.",
            "session_message_ack": "For retained session_attention requires_ack guidance, echo ack_session_message_ids. This request-scoped model-context proof neither resolves messages, grants authority, nor gates execution.",
            "session_message_resolution": "For a handled non-todo, send session_message_resolution on the next ordinary call with recording_session_id; ACK guidance also needs ack_session_message_ids. It cannot predict the main call. Todos use complete_session_message.",
            "context_sidecar": "context_request adds bounded context after the main tool and never authorizes effects. Recover lost project.instructions with an observation call before dependent mutation.",
            "runner_targeting": "For exact Runner client_id, use runtime_status(client_id=...) or list_projects(client_id=...) before treating it as absent.",
            "persistent_shell": "Persistent shell is primarily for repeated remote commands on one named SSH resource, preserving remote cwd/env/exports/functions/umask. Ordinary local coding stays on structured tools -> run_process/run_script -> run_shell for shell syntax; use local persistent shell only when same-process state is required.",
            "normal_closeout": "Normal success: finish_coding_task(summary_only=true); full closeout only for unresolved evidence or handoff/debug."
        },
        "roles": {
            "implementation_owner": {
                "purpose": "Implement one coherent change end to end.",
                "guidance": [
                    "Map cross-layer changes end to end; use compiler/schema/exhaustiveness failures to find missing integration.",
                    "Minimize concepts; fix concrete issues without speculative redesign.",
                    "When intentionally rerunning the same logical validation, reuse the same assertion_name; do not rerun solely to clear stale historical validation evidence."
                ]
            },
            "independent_review": {
                "purpose": "Review independently within task scope.",
                "guidance": [
                    "Challenge authority, bounds, malformed data, privacy, replay/races, timeouts, and fail-closed behavior.",
                    "For review-only tasks, report concrete findings with file/line evidence and impact; do not edit. Fix only when the task authorizes corrections, with focused regression validation."
                ]
            }
        }
    })
}

// Model-side caps for the deterministic repository overview projected into the
// shared startup brief. Lists are stable-sorted and record truncation; a list
// that exceeds its cap is never silently presented as complete.
pub(crate) const REPOSITORY_MAX_PROJECT_TYPES: usize = 8;
pub(crate) const REPOSITORY_MAX_MANIFESTS: usize = 12;
pub(crate) const REPOSITORY_MAX_KEY_FILES: usize = 16;
pub(crate) const REPOSITORY_MAX_TOP_LEVEL: usize = 24;
pub(crate) const REPOSITORY_MAX_SUGGESTED_READS: usize = 8;
pub(crate) const REPOSITORY_MAX_ROOTS_PER_CLASS: usize = 8;
pub(crate) const REPOSITORY_MAX_WARNINGS: usize = 8;
pub(crate) const REPOSITORY_MAX_PROJECT_TYPE_EVIDENCE: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StartupSkillEntry {
    pub(crate) skill_id: String,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) source_scope: String,
    pub(crate) trust: String,
    pub(crate) name_conflict: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StartupPluginEntry {
    pub(crate) plugin: String,
    pub(crate) name: String,
    pub(crate) tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(skip_serializing_if = "webcodex_core::plugin::PluginSelectionAnnotations::is_empty")]
    pub(crate) annotations: webcodex_core::plugin::PluginSelectionAnnotations,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StartupSkillsCatalog {
    pub(crate) status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason_code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) catalog_revision: Option<String>,
    pub(crate) total_count: usize,
    pub(crate) returned_count: usize,
    pub(crate) truncated: bool,
    pub(crate) entries: Vec<StartupSkillEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) discovery_hint: Option<&'static str>,
}

impl StartupSkillsCatalog {
    pub(crate) fn unavailable(reason_code: &'static str) -> Self {
        Self {
            status: "unavailable",
            reason_code: Some(reason_code),
            catalog_revision: None,
            total_count: 0,
            returned_count: 0,
            truncated: false,
            entries: Vec::new(),
            discovery_hint: Some(
                "Use skills.catalog or skill_list for explicit discovery when available.",
            ),
        }
    }

    pub(crate) fn available(
        catalog_revision: String,
        discovery_truncated: bool,
        entries: Vec<StartupSkillEntry>,
    ) -> Self {
        let total_count = entries.len();
        let mut returned = Vec::new();
        for entry in entries {
            let mut candidate = returned.clone();
            candidate.push(entry);
            let truncated = discovery_truncated || candidate.len() < total_count;
            let projection = Self {
                status: "available",
                reason_code: None,
                catalog_revision: Some(catalog_revision.clone()),
                total_count,
                returned_count: candidate.len(),
                truncated,
                entries: candidate.clone(),
                discovery_hint: truncated.then_some(
                    "Use skills.catalog or skill_list for broader or refreshed discovery.",
                ),
            };
            if serde_json::to_vec(&projection)
                .map(|bytes| bytes.len() <= STARTUP_SKILL_CATALOG_MAX_BYTES)
                .unwrap_or(false)
            {
                returned = candidate;
            } else {
                break;
            }
        }
        let truncated = discovery_truncated || returned.len() < total_count;
        Self {
            status: "available",
            reason_code: None,
            catalog_revision: Some(catalog_revision),
            total_count,
            returned_count: returned.len(),
            truncated,
            entries: returned,
            discovery_hint: truncated
                .then_some("Use skills.catalog or skill_list for broader or refreshed discovery."),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StartupPluginsCatalog {
    pub(crate) status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason_code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) catalog_revision: Option<String>,
    pub(crate) total_count: usize,
    pub(crate) returned_count: usize,
    pub(crate) truncated: bool,
    pub(crate) entries: Vec<StartupPluginEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) discovery_hint: Option<&'static str>,
}

impl StartupPluginsCatalog {
    pub(crate) fn unavailable(reason_code: &'static str) -> Self {
        Self {
            status: "unavailable",
            reason_code: Some(reason_code),
            catalog_revision: None,
            total_count: 0,
            returned_count: 0,
            truncated: false,
            entries: Vec::new(),
            discovery_hint: Some(
                "Use explicit plugin_tool list and describe when Plugin discovery is available.",
            ),
        }
    }

    pub(crate) fn available(
        catalog_revision: String,
        total_count: usize,
        entries: Vec<StartupPluginEntry>,
    ) -> Self {
        let mut returned = Vec::new();
        for entry in entries {
            let mut candidate = returned.clone();
            candidate.push(entry);
            let truncated = candidate.len() < total_count;
            let projection = Self {
                status: "available",
                reason_code: None,
                catalog_revision: Some(catalog_revision.clone()),
                total_count,
                returned_count: candidate.len(),
                truncated,
                entries: candidate.clone(),
                discovery_hint: truncated.then_some(
                    "Use plugins.catalog or explicit plugin_tool list and describe for broader or current schema discovery.",
                ),
            };
            if serde_json::to_vec(&projection)
                .map(|bytes| bytes.len() <= STARTUP_PLUGIN_CATALOG_MAX_BYTES)
                .unwrap_or(false)
            {
                returned = candidate;
            } else {
                break;
            }
        }
        let truncated = returned.len() < total_count;
        Self {
            status: "available",
            reason_code: None,
            catalog_revision: Some(catalog_revision),
            total_count,
            returned_count: returned.len(),
            truncated,
            entries: returned,
            discovery_hint: truncated.then_some(
                "Use plugins.catalog or explicit plugin_tool list and describe for broader or current schema discovery.",
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StartupExtensions {
    pub(crate) skills: StartupSkillsCatalog,
    pub(crate) plugins: StartupPluginsCatalog,
}

impl StartupExtensions {
    pub(crate) fn serialized_len(&self) -> usize {
        serde_json::to_vec(self)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX)
    }
}

pub(crate) fn bounded_extension_description(value: &str) -> String {
    if value.len() <= STARTUP_EXTENSION_DESCRIPTION_MAX_BYTES {
        return value.to_string();
    }
    let mut end = STARTUP_EXTENSION_DESCRIPTION_MAX_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

pub(crate) struct StartupBriefInput<'a> {
    pub(crate) detail: StartupDetail,
    pub(crate) requested_project: &'a str,
    pub(crate) project_resolution: &'a Value,
    pub(crate) resolved: &'a ResolvedProject,
    pub(crate) session: &'a SessionSummary,
    pub(crate) continuation_kind: &'a str,
    pub(crate) reused: bool,
    pub(crate) resume_requested: bool,
    pub(crate) instructions: &'a ProjectInstructionsSnapshot,
    pub(crate) previous_instructions: Option<&'a ProjectInstructionsSummarySnapshot>,
    pub(crate) force_instruction_load: bool,
    pub(crate) include_project_instructions: bool,
    pub(crate) include_reused_instruction_content: bool,
    pub(crate) extensions: Option<&'a StartupExtensions>,
    pub(crate) git: &'a Value,
    pub(crate) semantic_navigation: &'a Value,
    pub(crate) repository: &'a Value,
    pub(crate) continuation_feedback: &'a Value,
    pub(crate) active_jobs: &'a Value,
    pub(crate) owning_runner_available: Option<bool>,
    pub(crate) canonical_repository_root_matches: Option<bool>,
    pub(crate) runtime_status_call_failed: bool,
}

/// Build the one startup contract used by MCP, REST, and GPT Actions.
pub(crate) fn build_startup_brief(input: StartupBriefInput<'_>) -> Value {
    let minimal = input.detail == StartupDetail::Minimal;
    let instruction_projection = instructions_projection(
        input.instructions,
        input.previous_instructions,
        input.force_instruction_load,
        !minimal && input.include_project_instructions,
        input.include_reused_instruction_content,
        minimal,
    );
    let continuation = continuation_projection(
        input.continuation_feedback,
        input.active_jobs,
        minimal,
        input.continuation_kind,
    );
    let workspace = workspace_projection(input.git);
    let semantic_navigation = semantic_navigation_projection(input.semantic_navigation);
    let repository = repository_projection(input.repository);
    let (blockers, warnings) = startup_issues(
        &input,
        &instruction_projection,
        &workspace,
        &semantic_navigation,
    );
    let startup_verdict = startup_verdict(&blockers, &warnings, &continuation, minimal);
    let repository_identity = format!(
        "repository:v1:{}",
        canonical_repository_key(&input.resolved.config.path)
    );
    let mut brief = json!({
        "detail": input.detail.as_str(),
        "session": {
            "session_id": input.session.session_id,
            "mode": input.session.mode,
            "execution_context": input.session.execution_context,
            "continuation": input.continuation_kind,
            "reused": input.reused,
            "resume_requested": input.resume_requested,
            "explicit_resume_required_for_continuation": true,
        },
        "project": {
            "requested": input.requested_project,
            "resolved_id": input.resolved.resolved_id,
            "repository_identity": repository_identity,
            "canonical_repository_root_matches": input.canonical_repository_root_matches,
        },
        "project_resolution": input.project_resolution,
        "workspace": workspace,
        "workflow": builtin_coding_workflow_projection(),
        "instructions": instruction_projection,
        "continuation": continuation,
        "semantic_navigation": semantic_navigation,
        "repository": repository,
        "blockers": blockers,
        "warnings": warnings,
        "startup_verdict": startup_verdict,
        "deterministic": true,
        "llm_summary": false,
    });
    if let Some(extensions) = input.extensions {
        debug_assert!(extensions.serialized_len() <= STARTUP_EXTENSION_CATALOG_HARD_MAX_BYTES);
        brief["extensions"] = serde_json::to_value(extensions).unwrap_or_else(|_| {
            json!({
                "skills": StartupSkillsCatalog::unavailable("skills_catalog_unavailable"),
                "plugins": StartupPluginsCatalog::unavailable("plugin_runtime_unavailable"),
            })
        });
    }
    enforce_hard_size_limit(&mut brief);
    brief
}

/// Return the already-built shared core from any successful startup detail.
/// Standard/minimal are the core itself; full embeds the same core alongside
/// diagnostics. No transport is allowed to reassemble these fields.
pub(crate) fn startup_brief_from_output(output: &Value) -> Option<&Value> {
    if output.get("detail").and_then(Value::as_str) == Some("full") {
        output
            .get("startup_brief")
            .filter(|value| value.is_object())
    } else if output.is_object() {
        Some(output)
    } else {
        None
    }
}

fn workspace_projection(git: &Value) -> Value {
    let counts = git.get("counts").unwrap_or(&Value::Null);
    let git_available = git.get("available").and_then(Value::as_bool);
    let clean = git.get("clean").and_then(Value::as_bool);
    let conflicts = count(counts, "conflicted");
    let status = if conflicts > 0 {
        "blocked"
    } else if git_available == Some(false) || clean.is_none() {
        "unavailable"
    } else if clean == Some(true) {
        "clean"
    } else {
        "dirty"
    };
    let head = git
        .pointer("/head/commit")
        .cloned()
        .or_else(|| git.get("head").filter(|value| value.is_string()).cloned())
        .unwrap_or(Value::Null);
    json!({
        "status": status,
        "git_available": git_available,
        "branch": git.get("branch").cloned().unwrap_or(Value::Null),
        "head": head,
        "clean": clean,
        "conflicts": conflicts,
        "modified": count(counts, "modified"),
        "untracked": count(counts, "untracked"),
        "staged": count(counts, "staged"),
        "ahead": Value::Null,
        "behind": Value::Null,
    })
}

fn count(value: &Value, field: &str) -> u64 {
    value.get(field).and_then(Value::as_u64).unwrap_or(0)
}

fn semantic_navigation_projection(value: &Value) -> Value {
    json!({
        "supported": value
            .get("supported")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "status": value
            .get("status")
            .cloned()
            .unwrap_or_else(|| json!("probe_failed")),
        // Preserve an indeterminate startup observation as null. Coercing a
        // timed-out status probe to false would turn "not observed" into a
        // false semantic-navigation unavailability claim.
        "available": value
            .get("available")
            .filter(|available| available.is_boolean() || available.is_null())
            .cloned()
            .unwrap_or(Value::Null),
        "provider": value.get("server").cloned().unwrap_or(Value::Null),
        "capability": if value
            .get("supported")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            json!("lsp_read_only_navigation")
        } else {
            Value::Null
        },
        "reason_code": value.get("reason_code").cloned().unwrap_or(Value::Null),
    })
}

/// Deterministic repository overview for the startup brief. `source` is the
/// already-built overview (available) or an unavailable marker. Lists are
/// stable-sorted and bounded; a list that exceeds its cap records the original
/// total and flips truncated instead of silently presenting as complete.
pub(crate) fn repository_projection(source: &Value) -> Value {
    if source.get("status").and_then(Value::as_str) != Some("available") {
        let reason_code = if source.get("reason_code").and_then(Value::as_str)
            == Some(REPOSITORY_OVERVIEW_NOT_REQUESTED_REASON)
        {
            REPOSITORY_OVERVIEW_NOT_REQUESTED_REASON
        } else {
            "unsupported_or_unavailable"
        };
        return json!({
            "status": "unavailable",
            "reason_code": reason_code,
        });
    }
    json!({
        "status": "available",
        "reason_code": Value::Null,
        "project_types": bounded_list(
            source.get("project_types"),
            source
                .get("project_types")
                .and_then(Value::as_array)
                .map(Vec::len),
            false,
            REPOSITORY_MAX_PROJECT_TYPES,
            project_type_item,
        ),
        "manifests": bounded_list(
            source.get("manifests"),
            source.get("manifests").and_then(Value::as_array).map(Vec::len),
            false,
            REPOSITORY_MAX_MANIFESTS,
            manifest_item,
        ),
        "key_files": bounded_list(
            source.get("key_files"),
            source.get("key_files").and_then(Value::as_array).map(Vec::len),
            false,
            REPOSITORY_MAX_KEY_FILES,
            key_file_item,
        ),
        "roots": roots_projection(source.get("roots").unwrap_or(&Value::Null)),
        "top_level": bounded_list(
            source.get("top_level"),
            source.get("top_level").and_then(Value::as_array).map(Vec::len),
            false,
            REPOSITORY_MAX_TOP_LEVEL,
            top_level_item,
        ),
        "suggested_next_reads": bounded_list(
            source.get("suggested_next_reads"),
            source
                .get("suggested_next_reads")
                .and_then(Value::as_array)
                .map(Vec::len),
            false,
            REPOSITORY_MAX_SUGGESTED_READS,
            suggested_read_item,
        ),
        "scan": scan_projection(source.get("scan").unwrap_or(&Value::Null)),
        "warnings": bounded_string_items(source.get("warnings"), REPOSITORY_MAX_WARNINGS, 96)
            .into_iter()
            .map(Value::String)
            .collect::<Vec<_>>(),
    })
}

/// Project the fixed `scan` fields only. Never clone an arbitrary Runner
/// `scan` object: a malformed payload cannot smuggle extra fields into the
/// startup brief through this subobject. Missing or non-conforming fields
/// fall back to safe defaults rather than transparently echoing the source.
fn scan_projection(source: &Value) -> Value {
    let empty = serde_json::Map::new();
    let scan = source.as_object().unwrap_or(&empty);
    json!({
        "max_depth": scan.get("max_depth").and_then(Value::as_u64).unwrap_or(0),
        "limit": scan.get("limit").and_then(Value::as_u64).unwrap_or(0),
        "returned_entry_count": scan
            .get("returned_entry_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        "truncated": scan.get("truncated").and_then(Value::as_bool).unwrap_or(false),
        "truncation_reason": scan
            .get("truncation_reason")
            .cloned()
            .filter(|value| value.is_string() || value.is_null())
            .unwrap_or(Value::Null),
    })
}

fn project_type_item(value: &Value) -> Option<Value> {
    let kind = value.get("kind").and_then(Value::as_str)?;
    let evidence = value.get("evidence").and_then(Value::as_array);
    let evidence_total = evidence.map(Vec::len).unwrap_or(0);
    let items = evidence
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(REPOSITORY_MAX_PROJECT_TYPE_EVIDENCE)
        .map(|path| json!(bounded_json_string(path, MAX_PATH_JSON_BYTES).0))
        .collect::<Vec<_>>();
    Some(json!({
        "kind": kind,
        "evidence": items,
        "evidence_total": evidence_total,
        "evidence_truncated": evidence_total > items.len(),
    }))
}

fn manifest_item(value: &Value) -> Option<Value> {
    let path = value.get("path").and_then(Value::as_str)?;
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("manifest");
    Some(json!({
        "path": bounded_json_string(path, MAX_PATH_JSON_BYTES).0,
        "kind": kind,
    }))
}

fn key_file_item(value: &Value) -> Option<Value> {
    let path = value.get("path").and_then(Value::as_str)?;
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("key_file");
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Some(json!({
        "path": bounded_json_string(path, MAX_PATH_JSON_BYTES).0,
        "kind": kind,
        "reason": bounded_json_string(reason, 160).0,
    }))
}

fn top_level_item(value: &Value) -> Option<Value> {
    let path = value.get("path").and_then(Value::as_str)?;
    let kind = value.get("kind").and_then(Value::as_str).unwrap_or("file");
    Some(json!({
        "path": bounded_json_string(path, MAX_PATH_JSON_BYTES).0,
        "kind": kind,
    }))
}

fn suggested_read_item(value: &Value) -> Option<Value> {
    let path = value.get("path").and_then(Value::as_str)?;
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Some(json!({
        "path": bounded_json_string(path, MAX_PATH_JSON_BYTES).0,
        "reason": bounded_json_string(reason, 160).0,
    }))
}

fn roots_projection(source: &Value) -> Value {
    let mut projected = serde_json::Map::new();
    for class in ["source", "tests", "docs", "examples", "scripts", "ci"] {
        projected.insert(
            class.to_string(),
            bounded_list(
                source.get(class),
                source.get(class).and_then(Value::as_array).map(Vec::len),
                false,
                REPOSITORY_MAX_ROOTS_PER_CLASS,
                project_path_item,
            ),
        );
    }
    projected.insert(
        "classification_basis".to_string(),
        source
            .get("classification_basis")
            .cloned()
            .unwrap_or_else(|| json!("conventional_directory_name")),
    );
    Value::Object(projected)
}

pub(crate) fn project_instructions_context_projection(
    current: &ProjectInstructionsSnapshot,
) -> Value {
    // Sidecar requests are explicit current observations rather than Session
    // continuation deltas. Reuse the same bounded source/content/read_more
    // projection as coding startup, without consulting or mutating Session
    // instruction-retention state.
    instructions_projection(current, None, true, true, true, false)
}

fn instructions_projection(
    current: &ProjectInstructionsSnapshot,
    previous: Option<&ProjectInstructionsSummarySnapshot>,
    force_load: bool,
    allow_content: bool,
    include_reused_content: bool,
    minimal: bool,
) -> Value {
    let status = instruction_status(current, previous, force_load);
    let include_content = allow_content
        && (matches!(status, "loaded" | "changed")
            || (status == "reused" && include_reused_content)
            || (status == "unavailable" && !current.files.is_empty()));
    let changed_sources = if status == "changed" {
        changed_instruction_sources(current, previous)
    } else {
        Vec::new()
    };
    let mut remaining_content_budget = INSTRUCTION_CONTENT_JSON_BUDGET;
    let mut projection_truncated = false;
    let source_count = current.files.len();
    let sources: Vec<Value> = current
        .files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            instruction_source_projection(
                file,
                include_content,
                minimal,
                source_count.saturating_sub(index),
                &mut remaining_content_budget,
                &mut projection_truncated,
            )
        })
        .collect();
    json!({
        "status": status,
        "sources": sources,
        "changed_sources": changed_sources,
        "content_included": include_content,
        "truncated": current.truncated || projection_truncated,
        "total_chars": current.total_chars,
    })
}

fn instruction_status(
    current: &ProjectInstructionsSnapshot,
    previous: Option<&ProjectInstructionsSummarySnapshot>,
    force_load: bool,
) -> &'static str {
    if !current.scan_complete {
        return "unavailable";
    }
    if !current.loaded {
        return if previous.is_some_and(|snapshot| snapshot.loaded) {
            "changed"
        } else {
            "not_found"
        };
    }
    let Some(previous) = previous.filter(|snapshot| snapshot.scan_complete) else {
        return "loaded";
    };
    if force_load {
        return "loaded";
    }
    if instruction_snapshots_match(current, previous) {
        "reused"
    } else {
        "changed"
    }
}

fn instruction_snapshots_match(
    current: &ProjectInstructionsSnapshot,
    previous: &ProjectInstructionsSummarySnapshot,
) -> bool {
    current.loaded == previous.loaded
        && current.truncated == previous.truncated
        && current.total_chars == previous.total_chars
        && current.files.len() == previous.files.len()
        && current
            .files
            .iter()
            .zip(&previous.files)
            .all(|(left, right)| {
                left.path == right.path
                    && left.fingerprint == right.fingerprint
                    && left.truncated == right.truncated
            })
}

fn changed_instruction_sources(
    current: &ProjectInstructionsSnapshot,
    previous: Option<&ProjectInstructionsSummarySnapshot>,
) -> Vec<String> {
    let mut changed = Vec::new();
    for candidate in INSTRUCTION_CANDIDATE_PATHS {
        let current_file = current.files.iter().find(|file| file.path == *candidate);
        let previous_file = previous
            .and_then(|snapshot| snapshot.files.iter().find(|file| file.path == *candidate));
        let differs = match (current_file, previous_file) {
            (Some(left), Some(right)) => {
                left.fingerprint != right.fingerprint || left.truncated != right.truncated
            }
            (None, None) => false,
            _ => true,
        };
        if differs {
            changed.push((*candidate).to_string());
        }
    }
    changed
}

fn instruction_source_projection(
    file: &ProjectInstructionFile,
    include_content: bool,
    minimal: bool,
    remaining_sources: usize,
    remaining_content_budget: &mut usize,
    projection_truncated: &mut bool,
) -> Value {
    let headings = if minimal {
        Vec::new()
    } else {
        file.content
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('#'))
            .take(MAX_RULE_HEADINGS)
            .map(|line| bounded_json_string(line, MAX_RULE_HEADING_JSON_BYTES).0)
            .collect()
    };
    let (content, content_truncated) = if include_content {
        // Divide the remaining aggregate budget across the remaining sources.
        // Short earlier files leave their unused share available, while a long
        // earlier file cannot starve a later changed rule of all content.
        let source_budget = *remaining_content_budget / remaining_sources.max(1);
        let (content, truncated) = bounded_json_string(&file.content, source_budget);
        *remaining_content_budget =
            remaining_content_budget.saturating_sub(json_string_payload_len(&content));
        (Some(content), truncated)
    } else {
        (None, false)
    };
    *projection_truncated |= content_truncated;
    let read_more = if content_truncated {
        let returned = content.as_deref().unwrap_or_default();
        projected_read_more(&file.path, returned)
    } else {
        serde_json::to_value(&file.read_more).unwrap_or(Value::Null)
    };
    json!({
        "path": file.path,
        "fingerprint": file.fingerprint,
        "truncated": file.truncated || content_truncated,
        "headings": headings,
        "content": content,
        "read_more": read_more,
    })
}

fn projected_read_more(path: &str, returned: &str) -> Value {
    let observed_lines = returned.lines().count().max(1);
    let start_line = if returned.ends_with('\n') {
        observed_lines.saturating_add(1)
    } else {
        // The byte-bound projection may end midway through a line. Re-reading
        // that line is conservative and avoids losing its unseen suffix.
        observed_lines
    };
    json!({
        "path": path,
        "start_line": start_line,
        "limit": MAX_LINES_PER_FILE,
    })
}

fn continuation_projection(
    feedback: &Value,
    active_jobs: &Value,
    minimal: bool,
    continuation_kind: &str,
) -> Value {
    let status = feedback.get("status").and_then(Value::as_str).unwrap_or(
        if continuation_kind == "created" {
            "not_applicable"
        } else {
            "unknown"
        },
    );
    let attempt = feedback.get("attempt").unwrap_or(&Value::Null);
    let validation = attempt.get("validation").unwrap_or(&Value::Null);
    let delta = feedback.get("validation_delta").unwrap_or(&Value::Null);
    let delta_failures = delta.get("failures").unwrap_or(&Value::Null);
    let changes = attempt.get("changes").unwrap_or(&Value::Null);
    let exploration = attempt.get("exploration").unwrap_or(&Value::Null);
    let actions = attempt
        .get("suggested_next_actions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut projected = json!({
        "status": status,
        "reason_code": feedback.get("reason_code").cloned().unwrap_or(Value::Null),
        "instruction": {
            "status": attempt
                .pointer("/instruction/status")
                .cloned()
                .unwrap_or_else(|| json!("not_observed")),
            "excerpt": attempt
                .pointer("/instruction/excerpt")
                .and_then(Value::as_str)
                .map(|value| bounded_json_string(value, MAX_INSTRUCTION_EXCERPT_JSON_BYTES).0),
            "truncated": attempt
                .pointer("/instruction/truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "outcome": {
            "status": attempt
                .pointer("/outcome/status")
                .cloned()
                .unwrap_or_else(|| json!("unknown")),
            "reason_codes": bounded_string_items(
                attempt.pointer("/outcome/reason_codes"),
                8,
                96,
            ),
        },
        "changed_paths": bounded_list(
            changes.get("changed_paths"),
            changes
                .get("total_changed_paths")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
            changes
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            if minimal { 0 } else { MAX_CHANGED_PATHS },
            project_path_item,
        ),
        "exploration": {
            "paths": bounded_list(
                exploration.get("observed_paths"),
                exploration
                    .get("total_observed_paths")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize),
                exploration
                    .get("truncated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                if minimal {
                    MAX_MINIMAL_EXPLORATION_PATHS
                } else {
                    MAX_STANDARD_EXPLORATION_PATHS
                },
                exploration_path_item,
            ),
            "read_count": exploration
                .get("read_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            "search_count": exploration
                .get("search_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            "navigation_count": exploration
                .get("navigation_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            "latest_tool": exploration
                .get("latest_tool")
                .cloned()
                .unwrap_or(Value::Null),
            "complete": exploration
                .get("complete")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "validation": {
            "latest_status": normalized_validation_status(
                validation.get("latest_status").and_then(Value::as_str),
            ),
            "open_failures": bounded_list(
                validation.get("open_failures"),
                validation
                    .get("total_open_failures")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize),
                validation
                    .get("failures_truncated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                if minimal { 0 } else { MAX_FAILURES },
                failure_item,
            ),
            "delta": {
                "status": delta
                    .pointer("/comparison/status")
                    .cloned()
                    .unwrap_or_else(|| json!("unavailable")),
                "reason_code": delta
                    .pointer("/comparison/reason_code")
                    .cloned()
                    .unwrap_or(Value::Null),
                "new_failures": bounded_list(
                    delta_failures.get("newly_failed"),
                    delta_failures
                        .get("total_newly_failed")
                        .and_then(Value::as_u64)
                        .map(|value| value as usize),
                    false,
                    if minimal { 0 } else { MAX_FAILURES },
                    failure_item,
                ),
                "resolved_failures": bounded_list(
                    delta_failures.get("resolved"),
                    delta_failures
                        .get("total_resolved")
                        .and_then(Value::as_u64)
                        .map(|value| value as usize),
                    false,
                    if minimal { 0 } else { MAX_FAILURES },
                    failure_item,
                ),
                "still_failing": bounded_list(
                    delta_failures.get("still_failing"),
                    delta_failures
                        .get("total_still_failing")
                        .and_then(Value::as_u64)
                        .map(|value| value as usize),
                    false,
                    if minimal { 0 } else { MAX_FAILURES },
                    failure_item,
                ),
            },
        },
        "jobs": {
            "active_count": active_jobs
                .get("active_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            "blocking_active_count": active_jobs
                .get("blocking_active_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            "nonblocking_active_count": active_jobs
                .get("nonblocking_active_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            "recovering_count": active_jobs
                .get("recovering_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            "terminal_pending_count": active_jobs
                .get("terminal_pending_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            "latest_status": active_jobs
                .pointer("/recent/0/status")
                .cloned()
                .unwrap_or_else(|| json!("not_observed")),
        },
        "open_guidance": {
            "count": attempt
                .pointer("/guidance/open_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            "risk_count": attempt
                .pointer("/guidance/open_risk_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            "todo_count": attempt
                .pointer("/guidance/open_todo_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            "latest_kind": attempt
                .pointer("/guidance/latest_open_kind")
                .cloned()
                .unwrap_or(Value::Null),
        },
        "suggested_next_actions": bounded_list(
            Some(&Value::Array(actions)),
            None,
            false,
            if minimal { 1 } else { MAX_SUGGESTED_ACTIONS },
            action_item,
        ),
    });

    if active_jobs.get("active_job").is_some_and(Value::is_object) {
        projected["jobs"]["active_job"] = active_jobs["active_job"].clone();
    }

    if minimal {
        // The first action remains concrete, while bulk evidence lists are
        // represented only by their total/returned/truncated metadata.
        projected["instruction"]["excerpt"] = Value::Null;
    }
    projected
}

fn normalized_validation_status(status: Option<&str>) -> &'static str {
    match status {
        Some("passed") => "passed",
        Some("failed") => "failed",
        Some("expected") => "expected",
        Some("inconclusive") => "inconclusive",
        Some("not_run") => "not_run",
        Some("unavailable") => "unavailable",
        _ => "unknown",
    }
}

fn bounded_list(
    source: Option<&Value>,
    total: Option<usize>,
    source_truncated: bool,
    limit: usize,
    project: fn(&Value) -> Option<Value>,
) -> Value {
    let source_items = source.and_then(Value::as_array);
    let observed = source_items.map(Vec::len).unwrap_or(0);
    let total = total.unwrap_or(observed).max(observed);
    let items: Vec<Value> = source_items
        .into_iter()
        .flatten()
        .filter_map(project)
        .take(limit)
        .collect();
    let returned = items.len();
    json!({
        "items": items,
        "total": total,
        "returned": returned,
        "truncated": source_truncated || total > returned,
    })
}

fn project_path_item(value: &Value) -> Option<Value> {
    value
        .as_str()
        .map(|value| json!(bounded_json_string(value, MAX_PATH_JSON_BYTES).0))
}

fn exploration_path_item(value: &Value) -> Option<Value> {
    value.as_str().and_then(|value| {
        super::sessions::normalize_observed_project_path(value).map(Value::String)
    })
}

fn action_item(value: &Value) -> Option<Value> {
    value
        .as_str()
        .map(|value| json!(bounded_json_string(value, MAX_ACTION_JSON_BYTES).0))
}

fn failure_item(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    let kind = match object.get("kind").and_then(Value::as_str) {
        Some("test") => "test",
        Some("diagnostic") => "diagnostic",
        _ => "unknown",
    };
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .map(|value| bounded_json_string(value, MAX_FAILURE_NAME_JSON_BYTES).0)
        .unwrap_or_else(|| "unknown failure".to_string());
    let file = object
        .get("file")
        .and_then(Value::as_str)
        .map(|value| bounded_json_string(value, MAX_FAILURE_FILE_JSON_BYTES).0);
    Some(json!({
        "kind": kind,
        "name": name,
        "file": file,
        "line": object.get("line").cloned().unwrap_or(Value::Null),
    }))
}

fn bounded_string_items(source: Option<&Value>, limit: usize, budget: usize) -> Vec<String> {
    source
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(limit)
        .map(|value| bounded_json_string(value, budget).0)
        .collect()
}

fn startup_issues(
    input: &StartupBriefInput<'_>,
    instructions: &Value,
    workspace: &Value,
    semantic_navigation: &Value,
) -> (Vec<String>, Vec<String>) {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();
    if workspace
        .get("conflicts")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 0
    {
        push_unique(&mut blockers, "workspace_conflicts");
    }
    let blocking_jobs = input
        .active_jobs
        .get("blocking_active_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if blocking_jobs > 0 {
        push_unique(&mut blockers, "active_jobs_blocking");
    }
    if input.owning_runner_available == Some(false) {
        push_unique(&mut blockers, "runner_unavailable");
    }
    if input.runtime_status_call_failed {
        push_unique(&mut warnings, "runtime_status_unavailable");
    }
    if workspace.get("status").and_then(Value::as_str) == Some("dirty") {
        push_unique(&mut warnings, "dirty_worktree");
    }
    if workspace.get("git_available").and_then(Value::as_bool) == Some(false) {
        push_unique(&mut warnings, "git_unavailable");
    }
    if instructions.get("status").and_then(Value::as_str) == Some("unavailable") {
        push_unique(&mut warnings, "rules_unavailable");
    }
    if input.repository.get("status").and_then(Value::as_str) == Some("unavailable")
        && input.repository.get("reason_code").and_then(Value::as_str)
            != Some(REPOSITORY_OVERVIEW_NOT_REQUESTED_REASON)
    {
        push_unique(&mut warnings, "repository_overview_unavailable");
    }
    let semantic_status = semantic_navigation.get("status").and_then(Value::as_str);
    if semantic_navigation
        .get("available")
        .and_then(Value::as_bool)
        == Some(false)
        && !matches!(semantic_status, Some("not_applicable"))
    {
        push_unique(&mut warnings, "semantic_navigation_unavailable");
    }
    let active_jobs = input
        .active_jobs
        .get("active_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if active_jobs > 0 && blocking_jobs == 0 {
        push_unique(&mut warnings, "active_jobs_present");
    }
    (blockers, warnings)
}

fn startup_verdict(
    blockers: &[String],
    warnings: &[String],
    continuation: &Value,
    minimal: bool,
) -> Value {
    let mut actions = Vec::new();
    let continuation_actions = continuation
        .pointer("/suggested_next_actions/items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    if continuation
        .pointer("/validation/open_failures/total")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 0
    {
        if let Some(action) = continuation_actions.first() {
            push_unique(&mut actions, action);
        }
    }
    if blockers.iter().any(|item| item == "active_jobs_blocking") {
        push_unique(&mut actions, "inspect or await blocking active jobs");
    }
    if blockers.iter().any(|item| item == "runner_unavailable") {
        push_unique(
            &mut actions,
            "restore the project runner connection before attempting coding tools",
        );
    }
    if continuation
        .pointer("/open_guidance/risk_count")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 0
    {
        push_unique(
            &mut actions,
            "address open guidance on the session message board",
        );
    }
    if blockers.iter().any(|item| item == "workspace_conflicts") {
        push_unique(
            &mut actions,
            "resolve the existing workspace conflicts before editing unrelated code",
        );
    }
    for action in continuation_actions
        .iter()
        .copied()
        .filter(|action| *action != EXPLORATION_CONTINUITY_ACTION)
    {
        push_unique(&mut actions, action);
    }
    if warnings.iter().any(|item| item == "dirty_worktree") {
        push_unique(
            &mut actions,
            "inspect and preserve the existing worktree changes while continuing",
        );
    }
    if warnings.iter().any(|item| item == "rules_unavailable") {
        push_unique(
            &mut actions,
            "read the fixed repository instruction files before making changes",
        );
    }
    if continuation_actions.contains(&EXPLORATION_CONTINUITY_ACTION) {
        push_unique(&mut actions, EXPLORATION_CONTINUITY_ACTION);
    }
    if actions.is_empty() {
        actions.push("begin the requested coding task".to_string());
    }
    actions.truncate(if minimal { 1 } else { MAX_SUGGESTED_ACTIONS });
    let status = if !blockers.is_empty() {
        "fail"
    } else if !warnings.is_empty() {
        "warn"
    } else {
        "pass"
    };
    json!({
        "status": status,
        "blocking": !blockers.is_empty(),
        "suggested_next_actions": actions,
    })
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|existing| existing == value) {
        values.push(value.to_string());
    }
}

/// Bound by the serialized JSON payload cost, not Unicode scalar count. This
/// keeps control characters and escape-heavy content within the byte contract.
fn bounded_json_string(value: &str, max_payload_bytes: usize) -> (String, bool) {
    let mut output = String::new();
    let mut used = 0usize;
    for character in value.chars() {
        let encoded = serde_json::to_string(&character.to_string())
            .expect("single character JSON serialization");
        let cost = encoded.len().saturating_sub(2);
        if used.saturating_add(cost) > max_payload_bytes {
            return (output, true);
        }
        output.push(character);
        used = used.saturating_add(cost);
    }
    (output, false)
}

fn json_string_payload_len(value: &str) -> usize {
    serde_json::to_string(value)
        .map(|encoded| encoded.len().saturating_sub(2))
        .unwrap_or(0)
}

fn enforce_hard_size_limit(brief: &mut Value) {
    if serialized_len(brief) <= STANDARD_STARTUP_HARD_MAX_BYTES {
        return;
    }

    // Repository metadata is lower priority than rule prose: drop optional
    // repository list items (top_level, key_files, manifests, project-type
    // evidence, suggested reads, and per-class roots) before touching rule
    // content. Every list keeps its required fields and truncation metadata.
    const REPOSITORY_LIST_POINTERS: &[&str] = &[
        "/repository/top_level",
        "/repository/key_files",
        "/repository/manifests",
        "/repository/suggested_next_reads",
        "/repository/roots/source",
        "/repository/roots/tests",
        "/repository/roots/docs",
        "/repository/roots/examples",
        "/repository/roots/scripts",
        "/repository/roots/ci",
    ];
    loop {
        if serialized_len(brief) <= STANDARD_STARTUP_HARD_MAX_BYTES {
            return;
        }
        let mut removed = false;
        for pointer in REPOSITORY_LIST_POINTERS {
            let Some(list) = brief.pointer_mut(pointer) else {
                continue;
            };
            let Some(items) = list.get_mut("items").and_then(Value::as_array_mut) else {
                continue;
            };
            if items.len() > 1 {
                items.pop();
                list["returned"] = json!(items.len());
                list["truncated"] = json!(true);
                removed = true;
                break;
            }
        }
        // Shrink project-type evidence before its entries when all list
        // items are already at their floor.
        if !removed {
            let Some(types) = brief
                .pointer_mut("/repository/project_types/items")
                .and_then(Value::as_array_mut)
            else {
                break;
            };
            let mut shrank = false;
            for project_type in types.iter_mut().rev() {
                let Some(evidence) = project_type
                    .get_mut("evidence")
                    .and_then(Value::as_array_mut)
                else {
                    continue;
                };
                if evidence.len() > 1 {
                    evidence.pop();
                    project_type["evidence_truncated"] = json!(true);
                    shrank = true;
                    break;
                }
            }
            if !shrank {
                break;
            }
        }
    }

    // Rules content is the largest prose block. Shrink each source only to a
    // useful floor first, preserving content from every loaded source along
    // with source identity, headings, read_more, and truncation facts.
    loop {
        if serialized_len(brief) <= STANDARD_STARTUP_HARD_MAX_BYTES {
            return;
        }
        let Some(sources) = brief
            .pointer_mut("/instructions/sources")
            .and_then(Value::as_array_mut)
        else {
            break;
        };
        let Some(source) = sources.iter_mut().rev().find(|source| {
            source
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|content| {
                    json_string_payload_len(content) > MIN_INSTRUCTION_CONTENT_JSON_BYTES
                })
        }) else {
            break;
        };
        let content = source
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let current_budget = json_string_payload_len(content);
        let next_budget = current_budget
            .saturating_sub(512)
            .max(MIN_INSTRUCTION_CONTENT_JSON_BYTES);
        let (bounded, _) = bounded_json_string(content, next_budget);
        let path = source
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let read_more = projected_read_more(&path, &bounded);
        source["content"] = json!(bounded);
        source["truncated"] = json!(true);
        source["read_more"] = read_more;
        brief["instructions"]["truncated"] = json!(true);
    }

    // Extremely large continuation evidence is reduced item-by-item. Every
    // list retains the original total and flips truncated=true.
    const LIST_POINTERS: &[&str] = &[
        "/continuation/validation/delta/resolved_failures",
        "/continuation/validation/delta/new_failures",
        "/continuation/validation/delta/still_failing",
        "/continuation/validation/open_failures",
        "/continuation/exploration/paths",
        "/continuation/changed_paths",
    ];
    loop {
        if serialized_len(brief) <= STANDARD_STARTUP_HARD_MAX_BYTES {
            return;
        }
        let mut removed = false;
        for pointer in LIST_POINTERS {
            let Some(list) = brief.pointer_mut(pointer) else {
                continue;
            };
            let Some(items) = list.get_mut("items").and_then(Value::as_array_mut) else {
                continue;
            };
            if items.len() > 1 {
                items.pop();
                list["returned"] = json!(items.len());
                list["truncated"] = json!(true);
                removed = true;
                break;
            }
        }
        if !removed {
            break;
        }
    }

    // Headings are optional navigation metadata; source path/fingerprint and
    // rule content/read_more remain authoritative.
    loop {
        if serialized_len(brief) <= STANDARD_STARTUP_HARD_MAX_BYTES {
            return;
        }
        let Some(sources) = brief
            .pointer_mut("/instructions/sources")
            .and_then(Value::as_array_mut)
        else {
            break;
        };
        let Some(headings) = sources
            .iter_mut()
            .rev()
            .filter_map(|source| source.get_mut("headings"))
            .filter_map(Value::as_array_mut)
            .find(|headings| !headings.is_empty())
        else {
            break;
        };
        headings.pop();
    }

    // The bounded evidence above normally leaves the brief below the hard
    // limit. As a final defensive step, reduce rule excerpts below the useful
    // floor while retaining source metadata and a conservative read_more hint.
    loop {
        if serialized_len(brief) <= STANDARD_STARTUP_HARD_MAX_BYTES {
            return;
        }
        let Some(sources) = brief
            .pointer_mut("/instructions/sources")
            .and_then(Value::as_array_mut)
        else {
            break;
        };
        let Some(source) = sources.iter_mut().rev().find(|source| {
            source
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|content| !content.is_empty())
        }) else {
            break;
        };
        let content = source
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let current_budget = json_string_payload_len(content);
        let next_budget = current_budget.saturating_sub(128);
        let (bounded, _) = bounded_json_string(content, next_budget);
        let path = source
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let read_more = projected_read_more(&path, &bounded);
        source["content"] = json!(bounded);
        source["truncated"] = json!(true);
        source["read_more"] = read_more;
        brief["instructions"]["truncated"] = json!(true);
    }

    debug_assert!(
        serialized_len(brief) <= STANDARD_STARTUP_HARD_MAX_BYTES,
        "startup brief base contract exceeded its hard byte budget"
    );
}

fn serialized_len(value: &Value) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
pub(crate) fn startup_brief_size(value: &Value) -> usize {
    serialized_len(value)
}

#[cfg(test)]
pub(crate) fn instruction_source_paths(value: &Value) -> BTreeSet<String> {
    value
        .pointer("/instructions/sources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|source| source.get("path").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
pub(crate) fn validate_schema_instance_for_test(
    instance: &Value,
    schema: &Value,
) -> Result<(), String> {
    webcodex_tool_contracts::test_support::validate_schema_instance(instance, schema)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::ProjectConfig;
    use crate::tool_runtime::project_instructions::LoadedInstructionCandidate;
    use crate::tool_runtime::sessions::SessionGuards;
    use crate::tool_runtime::{SessionMode, ToolRuntime};

    #[test]
    fn semantic_navigation_projection_preserves_probe_timeout_as_unknown() {
        let source = json!({
            "supported": true,
            "available": Value::Null,
            "status": "probe_timeout",
            "server": Value::Null,
            "reason_code": "status_probe_timed_out",
        });
        let projection = semantic_navigation_projection(&source);
        assert_eq!(projection["supported"], true);
        assert_eq!(projection["available"], Value::Null);
        assert_eq!(projection["status"], "probe_timeout");
        assert_eq!(projection["provider"], Value::Null);
        assert_eq!(projection["capability"], "lsp_read_only_navigation");
        assert_eq!(projection["reason_code"], "status_probe_timed_out");
    }

    #[test]
    fn repository_scan_projection_keeps_only_fixed_fields() {
        // A malicious available overview smuggles oversized extras inside the
        // `scan` object. The projection must keep only the five fixed fields.
        let source = json!({
            "status": "available",
            "reason_code": Value::Null,
            "scan": {
                "max_depth": 2,
                "limit": 120,
                "returned_entry_count": 7,
                "truncated": true,
                "truncation_reason": "max_depth",
                "padding": "X".repeat(20_000),
                "nested": {"deep": "Y".repeat(10_000)},
            },
            "runner_secret": "/absolute/leak",
        });
        let projection = repository_projection(&source);
        let scan = &projection["scan"];
        assert!(scan.is_object());
        let mut keys: Vec<&str> = scan
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "limit",
                "max_depth",
                "returned_entry_count",
                "truncated",
                "truncation_reason"
            ]
        );
        let serialized = projection.to_string();
        assert!(!serialized.contains("padding"));
        assert!(!serialized.contains("runner_secret"));
        assert!(!serialized.contains("/absolute/leak"));
        assert!(
            serialized.len() <= STANDARD_STARTUP_HARD_MAX_BYTES,
            "scan projection exceeded hard limit: {}",
            serialized.len()
        );
    }

    #[test]
    fn repository_scan_projection_falls_back_safely_on_malformed_scan() {
        // A scan that is not an object, or whose typed fields are wrong, must
        // still project to the fixed shape with safe defaults rather than echo
        // the malformed source.
        let source = json!({
            "status": "available",
            "reason_code": Value::Null,
            "scan": "not-an-object",
        });
        let scan = &repository_projection(&source)["scan"];
        assert!(scan.is_object());
        assert_eq!(scan["max_depth"], 0);
        assert_eq!(scan["limit"], 0);
        assert_eq!(scan["returned_entry_count"], 0);
        assert_eq!(scan["truncated"], false);
        assert_eq!(scan["truncation_reason"], Value::Null);
    }

    fn failure(index: usize) -> Value {
        json!({
            "kind": if index.is_multiple_of(2) { "test" } else { "diagnostic" },
            "name": format!("very_long_failure_name_{index}_{}", "x".repeat(180)),
            "file": format!("src/generated/{index}/{}.rs", "y".repeat(180)),
            "line": index + 1,
        })
    }

    fn large_feedback() -> Value {
        let paths = (0..120)
            .map(|index| json!(format!("src/generated/{index}/{}.rs", "p".repeat(220))))
            .collect::<Vec<_>>();
        let observed_paths = (0..100)
            .map(|index| json!(format!("src/explored/{index}/{}.rs", "e".repeat(220))))
            .collect::<Vec<_>>();
        let failures = (0..30).map(failure).collect::<Vec<_>>();
        json!({
            "status": "available",
            "reason_code": Value::Null,
            "attempt": {
                "instruction": {
                    "status": "available",
                    "excerpt": "continue the exact failing target ".repeat(80),
                    "truncated": true,
                },
                "outcome": {
                    "status": "blocked",
                    "reason_codes": ["validation_failed"],
                },
                "changes": {
                    "changed_paths": paths,
                    "total_changed_paths": 120,
                    "truncated": false,
                },
                "exploration": {
                    "observed_paths": observed_paths,
                    "total_observed_paths": 140,
                    "truncated": true,
                    "read_count": 80,
                    "search_count": 40,
                    "navigation_count": 20,
                    "latest_tool": "goto_definition",
                    "complete": true,
                },
                "validation": {
                    "latest_status": "failed",
                    "open_failures": failures,
                    "total_open_failures": 30,
                    "failures_truncated": false,
                },
                "jobs": {
                    "active_count": 7,
                    "recovering_count": 2,
                    "terminal_pending_count": 1,
                    "latest_job_status": "recovering",
                },
                "guidance": {
                    "open_count": 9,
                    "open_risk_count": 4,
                    "open_todo_count": 5,
                    "latest_open_kind": "risk",
                },
                "suggested_next_actions": (0..12)
                    .map(|index| format!("rerun focused target {index} {}", "a".repeat(420)))
                    .collect::<Vec<_>>(),
            },
            "validation_delta": {
                "comparison": {
                    "status": "available",
                    "reason_code": Value::Null,
                },
                "failures": {
                    "newly_failed": (0..25).map(failure).collect::<Vec<_>>(),
                    "total_newly_failed": 25,
                    "resolved": (25..50).map(failure).collect::<Vec<_>>(),
                    "total_resolved": 25,
                    "still_failing": (50..75).map(failure).collect::<Vec<_>>(),
                    "total_still_failing": 25,
                    "list_truncated": false,
                },
            },
        })
    }

    fn instruction_snapshot() -> ProjectInstructionsSnapshot {
        ProjectInstructionsSnapshot::from_candidates(
            INSTRUCTION_CANDIDATE_PATHS
                .iter()
                .enumerate()
                .map(|(index, path)| LoadedInstructionCandidate {
                    path: (*path).to_string(),
                    content: format!(
                        "# Rule source {index}\n## Required\n{}\n",
                        format!("bounded rule {index}; ").repeat(1_000)
                    ),
                    total_lines: 3,
                    full_sha256: None,
                })
                .collect(),
            true,
        )
    }

    #[test]
    fn reused_instruction_status_and_body_projection_are_independent() {
        let current = instruction_snapshot();
        let previous = current.to_summary();

        let advanced =
            instructions_projection(&current, Some(&previous), false, true, false, false);
        assert_eq!(advanced["status"], "reused");
        assert_eq!(advanced["content_included"], false);
        assert!(advanced["sources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|source| source["content"].is_null()));

        let canonical =
            instructions_projection(&current, Some(&previous), false, true, true, false);
        assert_eq!(canonical["status"], "reused");
        assert_eq!(canonical["content_included"], true);
        assert!(canonical["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["content"].is_string()));
    }

    fn delta_feedback(new_total: usize, resolved_total: usize, still_total: usize) -> Value {
        let failures = |total: usize, offset: usize| {
            (0..total.min(20))
                .map(|index| failure(offset + index))
                .collect::<Vec<_>>()
        };
        json!({
            "status": "available",
            "attempt": {},
            "validation_delta": {
                "comparison": {"status": "available", "reason_code": Value::Null},
                "failures": {
                    "newly_failed": failures(new_total, 0),
                    "total_newly_failed": new_total,
                    "resolved": failures(resolved_total, 100),
                    "total_resolved": resolved_total,
                    "still_failing": failures(still_total, 200),
                    "total_still_failing": still_total,
                    "list_truncated": true,
                }
            }
        })
    }

    fn empty_active_jobs() -> Value {
        json!({
            "active_count": 0,
            "blocking_active_count": 0,
            "nonblocking_active_count": 0,
            "recovering_count": 0,
            "terminal_pending_count": 0,
            "recent": [],
        })
    }

    fn large_repository() -> Value {
        let types = (0..12)
            .map(|index| {
                json!({
                    "kind": format!("kind_{index}"),
                    "evidence": (0..6)
                        .map(|j| format!("src/generated/{index}/{j}.rs"))
                        .collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        let manifests = (0..40)
            .map(|index| {
                json!({
                    "path": format!("crates/generated-{index}/Cargo.toml"),
                    "kind": "rust_manifest",
                })
            })
            .collect::<Vec<_>>();
        let key_files = (0..60)
            .map(|index| {
                json!({
                    "path": format!("generated/key_{index}.md"),
                    "kind": "documentation",
                    "reason": format!("generated key file {index} {}", "r".repeat(200)),
                })
            })
            .collect::<Vec<_>>();
        let top_level = (0..80)
            .map(|index| {
                json!({
                    "path": format!("generated/top_level_{index}.rs"),
                    "kind": "file",
                })
            })
            .collect::<Vec<_>>();
        let suggested = (0..20)
            .map(|index| {
                json!({
                    "path": format!("generated/read_{index}.rs"),
                    "reason": format!("generated read reason {index}"),
                })
            })
            .collect::<Vec<_>>();
        let roots = |prefix: &str| {
            (0..30)
                .map(|index| json!(format!("{prefix}/generated-{index}")))
                .collect::<Vec<_>>()
        };
        json!({
            "status": "available",
            "reason_code": Value::Null,
            "project_types": types,
            "manifests": manifests,
            "key_files": key_files,
            "roots": {
                "source": roots("src"),
                "tests": roots("tests"),
                "docs": roots("docs"),
                "examples": roots("examples"),
                "scripts": roots("scripts"),
                "ci": roots(".github"),
                "classification_basis": "conventional_directory_name",
            },
            "top_level": top_level,
            "suggested_next_reads": suggested,
            "scan": {
                "max_depth": 2,
                "limit": 120,
                "returned_entry_count": 200,
                "truncated": true,
                "truncation_reason": "limit",
            },
            "warnings": ["symlinks_skipped", "non_utf8_paths_skipped"],
        })
    }

    #[test]
    fn startup_exploration_projection_uses_minimal_and_standard_path_limits() {
        let feedback = json!({
            "status": "available",
            "attempt": {
                "exploration": {
                    "observed_paths": (0..20)
                        .map(|index| format!("src/explored-{index:02}.rs"))
                        .collect::<Vec<_>>(),
                    "total_observed_paths": 20,
                    "truncated": false,
                    "read_count": 3,
                    "search_count": 2,
                    "navigation_count": 4,
                    "latest_tool": "read_files",
                    "complete": true
                }
            },
            "validation_delta": {}
        });
        let minimal = continuation_projection(&feedback, &empty_active_jobs(), true, "continued");
        let standard = continuation_projection(&feedback, &empty_active_jobs(), false, "continued");

        assert_eq!(
            minimal["exploration"]["paths"],
            json!({
                "items": ["src/explored-00.rs", "src/explored-01.rs", "src/explored-02.rs"],
                "total": 20,
                "returned": 3,
                "truncated": true
            })
        );
        assert_eq!(standard["exploration"]["paths"]["returned"], 12);
        assert_eq!(standard["exploration"]["paths"]["total"], 20);
        assert_eq!(standard["exploration"]["paths"]["truncated"], true);
        assert_eq!(standard["exploration"]["read_count"], 3);
        assert_eq!(standard["exploration"]["search_count"], 2);
        assert_eq!(standard["exploration"]["navigation_count"], 4);
        assert_eq!(standard["exploration"]["latest_tool"], "read_files");
        assert_eq!(standard["exploration"]["complete"], true);
    }

    #[test]
    fn continuation_projection_preserves_only_explicit_active_job_handle() {
        let feedback = json!({"status": "available", "attempt": {}, "validation_delta": {}});
        let mut jobs = empty_active_jobs();
        assert!(
            continuation_projection(&feedback, &jobs, true, "continued")["jobs"]
                .get("active_job")
                .is_none()
        );

        jobs["active_job"] = json!({
            "job_id": "job-exact",
            "status": "running",
            "kind": "shell"
        });
        let projected = continuation_projection(&feedback, &jobs, true, "continued");
        assert_eq!(
            projected["jobs"]["active_job"],
            json!({"job_id": "job-exact", "status": "running", "kind": "shell"})
        );
        assert_eq!(projected["jobs"]["active_count"], 0);
    }

    #[test]
    fn validation_delta_truncates_only_still_failing_when_only_it_exceeds_limit() {
        let projected = continuation_projection(
            &delta_feedback(0, 1, 25),
            &empty_active_jobs(),
            false,
            "continued",
        );
        let delta = &projected["validation"]["delta"];
        assert_eq!(
            delta["new_failures"],
            json!({"items": [], "total": 0, "returned": 0, "truncated": false})
        );
        assert_eq!(delta["resolved_failures"]["total"], 1);
        assert_eq!(delta["resolved_failures"]["returned"], 1);
        assert_eq!(delta["resolved_failures"]["truncated"], false);
        assert_eq!(delta["still_failing"]["total"], 25);
        assert_eq!(delta["still_failing"]["returned"], 10);
        assert_eq!(delta["still_failing"]["truncated"], true);
    }

    #[test]
    fn validation_delta_truncates_only_new_failures_when_only_it_exceeds_limit() {
        let projected = continuation_projection(
            &delta_feedback(25, 1, 0),
            &empty_active_jobs(),
            false,
            "continued",
        );
        let delta = &projected["validation"]["delta"];
        assert_eq!(delta["new_failures"]["total"], 25);
        assert_eq!(delta["new_failures"]["returned"], 10);
        assert_eq!(delta["new_failures"]["truncated"], true);
        assert_eq!(delta["resolved_failures"]["total"], 1);
        assert_eq!(delta["resolved_failures"]["returned"], 1);
        assert_eq!(delta["resolved_failures"]["truncated"], false);
        assert_eq!(
            delta["still_failing"],
            json!({"items": [], "total": 0, "returned": 0, "truncated": false})
        );
    }

    #[test]
    fn standard_coding_task_startup_is_deterministic_and_hard_bounded() {
        let runtime = ToolRuntime::new_for_tests();
        let session = runtime.sessions.start_session_with_guards(
            Some("agent:size:demo".to_string()),
            Some("bounded startup".to_string()),
            SessionMode::Normal,
            SessionGuards::default(),
        );
        let resolved = ResolvedProject {
            input: "agent:size:demo".to_string(),
            resolved_id: "agent:size:demo".to_string(),
            config: ProjectConfig {
                path: "/tmp/model-facing-size-fixture".to_string(),
                client_id: "size".to_string(),
                allow_patch: true,
            },
        };
        let instructions = instruction_snapshot();
        let git = json!({
            "available": true,
            "branch": "main",
            "head": {"commit": "a".repeat(40)},
            "clean": false,
            "counts": {
                "conflicted": 0,
                "modified": 120,
                "untracked": 4,
                "staged": 3,
            },
        });
        let semantic_navigation = json!({
            "supported": true,
            "available": true,
            "status": "running",
            "server": "rust-analyzer",
            "reason_code": Value::Null,
        });
        let feedback = large_feedback();
        let active_jobs = json!({
            "active_count": 7,
            "blocking_active_count": 6,
            "nonblocking_active_count": 1,
            "recovering_count": 2,
            "terminal_pending_count": 1,
            "recent": [{"status": "recovering"}],
        });
        let extensions = StartupExtensions {
            skills: StartupSkillsCatalog::available(
                format!("wc_skillcat_{}", "a".repeat(64)),
                false,
                (0..64)
                    .map(|index| StartupSkillEntry {
                        skill_id: format!("wc_skill_{index:032x}"),
                        name: format!("skill-{index:02}"),
                        description: format!("skill-{index:02}-{}", "s".repeat(500)),
                        source_scope: "project".to_string(),
                        trust: "project_content".to_string(),
                        name_conflict: index < 2,
                    })
                    .collect(),
            ),
            plugins: StartupPluginsCatalog::available(
                format!("wc_plugcat_{}", "b".repeat(64)),
                64,
                (0..64)
                    .map(|index| StartupPluginEntry {
                        plugin: format!("plugin-{index:02}"),
                        name: format!("Plugin {index:02}"),
                        tool: format!("tool_{index:02}"),
                        title: Some(format!("Tool {index:02}")),
                        description: Some(format!("plugin-{index:02}-{}", "p".repeat(500))),
                        annotations: webcodex_core::plugin::PluginSelectionAnnotations {
                            read_only_hint: Some(true),
                            destructive_hint: Some(false),
                            idempotent_hint: Some(true),
                            open_world_hint: Some(false),
                        },
                    })
                    .collect(),
            ),
        };
        assert!(extensions.serialized_len() <= STARTUP_EXTENSION_CATALOG_HARD_MAX_BYTES);
        assert!(extensions.skills.truncated);
        assert!(extensions.plugins.truncated);
        let project_resolution = json!({
            "source": "project",
            "outcome": "resolved_existing_project",
            "resolved_project": "agent:size:demo",
            "registered": false,
        });
        let build = || {
            build_startup_brief(StartupBriefInput {
                detail: StartupDetail::Standard,
                requested_project: "agent:size:demo",
                project_resolution: &project_resolution,
                resolved: &resolved,
                session: &session,
                continuation_kind: "continued",
                reused: true,
                resume_requested: false,
                instructions: &instructions,
                previous_instructions: None,
                force_instruction_load: true,
                include_project_instructions: true,
                include_reused_instruction_content: false,
                extensions: Some(&extensions),
                git: &git,
                semantic_navigation: &semantic_navigation,
                repository: &large_repository(),
                continuation_feedback: &feedback,
                active_jobs: &active_jobs,
                owning_runner_available: Some(true),
                canonical_repository_root_matches: Some(true),
                runtime_status_call_failed: false,
            })
        };
        let first = build();
        let second = build();
        assert_eq!(first, second);
        let bytes = startup_brief_size(&first);
        eprintln!("worst_case_standard_startup_bytes={bytes}");
        assert!(bytes <= STANDARD_STARTUP_HARD_MAX_BYTES, "{bytes}");
        let action_wrapped_bytes = serde_json::to_vec(&json!({
            "success": true,
            "output": {
                "compact": true,
                "startup_brief": first.clone(),
            },
            "error": Value::Null,
        }))
        .unwrap()
        .len();
        eprintln!("worst_case_action_wrapped_startup_bytes={action_wrapped_bytes}");
        assert!(action_wrapped_bytes < 32 * 1024, "{action_wrapped_bytes}");
        assert_eq!(
            instruction_source_paths(&first),
            INSTRUCTION_CANDIDATE_PATHS
                .iter()
                .map(|path| (*path).to_string())
                .collect()
        );
        assert_eq!(first["instructions"]["content_included"], true);
        assert_eq!(first["instructions"]["truncated"], true);
        for source in first["instructions"]["sources"].as_array().unwrap() {
            assert!(
                source["content"]
                    .as_str()
                    .is_some_and(|content| !content.is_empty()),
                "each loaded rule source must retain a usable bounded excerpt: {source}"
            );
            assert!(source["read_more"].is_object());
        }
        assert_eq!(first["continuation"]["changed_paths"]["total"], 120);
        assert!(
            first["continuation"]["changed_paths"]["returned"]
                .as_u64()
                .unwrap()
                <= 20
        );
        assert_eq!(first["continuation"]["changed_paths"]["truncated"], true);
        assert_eq!(first["continuation"]["exploration"]["paths"]["total"], 140);
        assert!(
            first["continuation"]["exploration"]["paths"]["returned"]
                .as_u64()
                .unwrap()
                <= MAX_STANDARD_EXPLORATION_PATHS as u64
        );
        assert_eq!(
            first["continuation"]["exploration"]["paths"]["truncated"],
            true
        );
        for pointer in [
            "/continuation/validation/open_failures",
            "/continuation/validation/delta/new_failures",
            "/continuation/validation/delta/resolved_failures",
            "/continuation/validation/delta/still_failing",
        ] {
            let list = first.pointer(pointer).unwrap();
            assert!(list["returned"].as_u64().unwrap() <= 10, "{pointer}");
            assert_eq!(list["truncated"], true, "{pointer}");
        }
        assert!(
            first["continuation"]["suggested_next_actions"]["returned"]
                .as_u64()
                .unwrap()
                <= 5
        );
        assert!(first["continuation"]["suggested_next_actions"]["items"][0]
            .as_str()
            .unwrap()
            .starts_with("rerun focused target 0"));
        assert!(first["startup_verdict"]["suggested_next_actions"][0]
            .as_str()
            .unwrap()
            .starts_with("rerun focused target 0"));
        assert!(first["startup_verdict"]["suggested_next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .any(|action| action.starts_with("inspect or await blocking active jobs")));
        assert!(first["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "active_jobs_blocking"));
        assert!(first["startup_verdict"]["suggested_next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .any(|action| action.starts_with("rerun focused target 0")));
    }
}
