//! Runner-side read-only LSP navigation operations.
//!
//! Resolves project roots under policy, talks to `LspSupervisor`, normalizes
//! locations to project-relative paths, and never returns absolute paths,
//! file URIs, or executable paths to the model.

use super::super::config::RunnerPolicy;
use super::super::output::CommandResult;
use super::super::projects::load_runner_project_summaries_from_dir;
use super::super::shell::cwd_allowed;
use super::language::{
    detected_profiles, primary_profile, route_extension, supported_extensions_label,
    LanguageProfile, LANGUAGES,
};
use super::position::{lsp_to_public, public_to_lsp, LineCache, MAX_LSP_DOCUMENT_BYTES};
use super::supervisor::{
    classify_uri_against_project_root, LspError, LspServerStatus, LspSupervisor, PositionEncoding,
    ProjectUriClassification,
};
#[cfg(test)]
use crate::lsp_bridge::AGENT_LSP_REQUEST_KIND;
use crate::lsp_bridge::{
    bound_error_message, error_codes, redact_absolute_paths, validate_call_hierarchy_bounds,
    CallHierarchyDirection, CallHierarchyEdgeDirection, CallHierarchyResult,
    DocumentDiagnosticsResult, DocumentDiagnosticsStatus, DocumentSymbolsResult, HoverResult,
    LocationsResult, LspAvailabilityStatus, LspServerStatusEntry, LspStatusResult,
    PublicCallHierarchyEdge, PublicCallHierarchySymbol, PublicDiagnostic, PublicHover,
    PublicLocation, PublicPosition, PublicRange, PublicSymbol, PublicWorkspaceSymbol,
    RunnerLspPayload, RunnerLspRequest, RunnerLspResultEnvelope, WorkspaceSymbolsResult,
    MAX_CALL_HIERARCHY_CALL_ENTRIES_INSPECTED_PER_RPC, MAX_CALL_HIERARCHY_CALL_SITES_PER_EDGE,
    MAX_CALL_HIERARCHY_PREPARE_ITEMS_INSPECTED,
    MAX_CALL_HIERARCHY_RAW_CALL_SITE_RANGES_INSPECTED_PER_ENTRY, MAX_CALL_HIERARCHY_ROOTS,
};
#[cfg(test)]
use crate::runner_protocol::RunnerRequest;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use url::Url;

const MAX_SYMBOL_NAME_CHARS: usize = 256;
const MAX_SYMBOL_DETAIL_CHARS: usize = 512;
const MAX_DIAGNOSTIC_MESSAGE_CHARS: usize = 4096;
const MAX_DIAGNOSTIC_SOURCE_CHARS: usize = 128;
const MAX_DIAGNOSTIC_CODE_CHARS: usize = 128;
const MAX_DIAGNOSTIC_TOTAL_TEXT_CHARS: usize = 64 * 1024;
const DIAGNOSTICS_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_HOVER_VALUE_CHARS: usize = 16 * 1024;
const MAX_WORKSPACE_SYMBOL_FIELD_CHARS: usize = 256;
#[cfg(test)]
pub(crate) fn is_lsp_request_kind(kind: &str) -> bool {
    kind == AGENT_LSP_REQUEST_KIND
}

pub(crate) fn handle_lsp_operation(
    policy: &RunnerPolicy,
    project_registry_dir: &Path,
    supervisor: &LspSupervisor,
    payload: &RunnerLspPayload,
    timeout_secs: u64,
) -> CommandResult {
    let start = Instant::now();
    let operation_deadline = start
        .checked_add(Duration::from_secs(timeout_secs.max(1)))
        .unwrap_or(start);
    match execute_lsp(
        policy,
        project_registry_dir,
        supervisor,
        payload,
        operation_deadline,
    ) {
        Ok(envelope) => CommandResult {
            // Always exit 0 for structured envelopes so the server can parse
            // success/failure from the versioned JSON rather than shell status.
            exit_code: Some(0),
            stdout: Some(envelope.to_stdout_json()),
            stderr: Some(String::new()),
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: None,
        },
        Err(envelope) => CommandResult {
            exit_code: Some(0),
            stdout: Some(envelope.to_stdout_json()),
            stderr: Some(String::new()),
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: None,
        },
    }
}

#[cfg(test)]
pub(crate) fn handle_lsp_request(
    policy: &RunnerPolicy,
    project_registry_dir: &Path,
    supervisor: &LspSupervisor,
    request: &RunnerRequest,
) -> CommandResult {
    let Some(payload) = request.lsp.as_ref() else {
        return lsp_error_cmd(
            Instant::now(),
            error_codes::MISSING_LSP_PAYLOAD,
            "LSP request missing typed payload",
        );
    };
    handle_lsp_operation(
        policy,
        project_registry_dir,
        supervisor,
        payload,
        request.timeout_secs,
    )
}

fn execute_lsp(
    policy: &RunnerPolicy,
    project_registry_dir: &Path,
    supervisor: &LspSupervisor,
    payload: &RunnerLspPayload,
    operation_deadline: Instant,
) -> Result<RunnerLspResultEnvelope, RunnerLspResultEnvelope> {
    let project = resolve_runner_project(project_registry_dir, &payload.project_id)?;
    let project_root = validate_project_root(policy, &project.path)?;
    match &payload.request {
        RunnerLspRequest::Status => Ok(RunnerLspResultEnvelope::ok(lsp_status(
            &payload.project_id,
            &project_root,
            supervisor,
        ))),
        RunnerLspRequest::DocumentSymbols { path, limit } => {
            let result =
                document_symbols(&payload.project_id, &project_root, supervisor, path, *limit)?;
            Ok(RunnerLspResultEnvelope::ok(result))
        }
        RunnerLspRequest::DocumentDiagnostics { path, limit } => {
            let result =
                document_diagnostics(&payload.project_id, &project_root, supervisor, path, *limit)?;
            Ok(RunnerLspResultEnvelope::ok(result))
        }
        RunnerLspRequest::Hover { path, line, column } => {
            let result = hover(
                &payload.project_id,
                &project_root,
                supervisor,
                path,
                *line,
                *column,
            )?;
            Ok(RunnerLspResultEnvelope::ok(result))
        }
        RunnerLspRequest::WorkspaceSymbols { query, limit } => {
            let result = workspace_symbols(
                &payload.project_id,
                &project_root,
                supervisor,
                query,
                *limit,
                operation_deadline,
            )?;
            Ok(RunnerLspResultEnvelope::ok(result))
        }
        RunnerLspRequest::GotoDefinition {
            path,
            line,
            column,
            limit,
        } => {
            let result = goto_definition(
                &payload.project_id,
                &project_root,
                supervisor,
                path,
                *line,
                *column,
                *limit,
            )?;
            Ok(RunnerLspResultEnvelope::ok(result))
        }
        RunnerLspRequest::FindReferences {
            path,
            line,
            column,
            include_declaration,
            limit,
        } => {
            let result = find_references(
                &payload.project_id,
                &project_root,
                supervisor,
                path,
                *line,
                *column,
                *include_declaration,
                *limit,
            )?;
            Ok(RunnerLspResultEnvelope::ok(result))
        }
        RunnerLspRequest::CallHierarchy {
            path,
            line,
            column,
            direction,
            depth,
            limit,
        } => {
            let result = call_hierarchy(
                &payload.project_id,
                &project_root,
                supervisor,
                path,
                *line,
                *column,
                *direction,
                *depth,
                *limit,
                operation_deadline,
            )?;
            Ok(RunnerLspResultEnvelope::ok(result))
        }
    }
}

struct ResolvedProject {
    path: PathBuf,
}

fn resolve_runner_project(
    project_registry_dir: &Path,
    project_id: &str,
) -> Result<ResolvedProject, RunnerLspResultEnvelope> {
    let id = project_id.trim();
    if id.is_empty() {
        return Err(RunnerLspResultEnvelope::err(
            error_codes::UNKNOWN_PROJECT,
            "project_id cannot be empty",
        ));
    }
    let projects = load_runner_project_summaries_from_dir(project_registry_dir);
    let project = projects.into_iter().find(|p| p.id == id).ok_or_else(|| {
        RunnerLspResultEnvelope::err(error_codes::UNKNOWN_PROJECT, "unknown agent project")
    })?;
    Ok(ResolvedProject {
        path: PathBuf::from(project.path),
    })
}

fn validate_project_root(
    policy: &RunnerPolicy,
    path: &Path,
) -> Result<PathBuf, RunnerLspResultEnvelope> {
    cwd_allowed(policy, path).map_err(|message| {
        RunnerLspResultEnvelope::err(
            error_codes::INVALID_PROJECT_PATH,
            sanitize_path_message(message),
        )
    })?;
    fs::canonicalize(path).map_err(|_| {
        RunnerLspResultEnvelope::err(
            error_codes::INVALID_PROJECT_PATH,
            "project root is not accessible",
        )
    })
}

fn lsp_status(
    project_id: &str,
    project_root: &Path,
    supervisor: &LspSupervisor,
) -> LspStatusResult {
    let detected_languages = detected_profiles(project_root)
        .iter()
        .map(|profile| profile.language_id.to_string())
        .collect();
    let servers = LANGUAGES
        .iter()
        .map(|profile| lsp_server_status_entry(project_root, supervisor, profile))
        .collect();
    LspStatusResult {
        project: project_id.to_string(),
        detected_languages,
        servers,
        warnings: Vec::new(),
    }
}

fn lsp_server_status_entry(
    project_root: &Path,
    supervisor: &LspSupervisor,
    profile: &'static LanguageProfile,
) -> LspServerStatusEntry {
    let info = supervisor.resolve_command_info(profile.kind);
    let available = info.as_ref().map(|entry| entry.available).unwrap_or(false);
    let slot = supervisor.project_server_status(project_root, profile.kind);
    let (running, status, position_encoding) = match slot {
        Some(LspServerStatus::Running) => {
            let encoding = supervisor
                .project_position_encoding(project_root, profile.kind)
                .map(|encoding| encoding.as_public_label().to_string());
            (true, LspAvailabilityStatus::Running, encoding)
        }
        Some(LspServerStatus::Initializing) => (false, LspAvailabilityStatus::Initializing, None),
        Some(LspServerStatus::Crashed) => (false, LspAvailabilityStatus::Crashed, None),
        None => {
            if available {
                (false, LspAvailabilityStatus::Available, None)
            } else {
                (false, LspAvailabilityStatus::Unavailable, None)
            }
        }
    };
    let source = info.as_ref().map(|entry| entry.source);
    LspServerStatusEntry {
        language: profile.language_id.to_string(),
        server: profile.server_name.to_string(),
        available,
        running,
        status,
        source,
        position_encoding,
    }
}

/// Read a validated project document with a pre-allocation size guard.
///
/// The LSP wire cap (`MAX_LSP_MESSAGE_BYTES`) would reject an oversized
/// `didOpen` only after the whole file is already resident in agent memory;
/// checking metadata first keeps a model-chosen giant `.rs` file from forcing
/// that allocation. See `MAX_LSP_DOCUMENT_BYTES` for the race caveat.
fn read_document_text(file: &Path) -> Result<String, RunnerLspResultEnvelope> {
    let metadata = fs::metadata(file).map_err(|_| {
        RunnerLspResultEnvelope::err(error_codes::FILE_NOT_FOUND, "failed to read file")
    })?;
    if metadata.len() > MAX_LSP_DOCUMENT_BYTES {
        return Err(RunnerLspResultEnvelope::err(
            error_codes::DOCUMENT_TOO_LARGE,
            "file exceeds the LSP navigation document size limit",
        ));
    }
    fs::read_to_string(file).map_err(|_| {
        RunnerLspResultEnvelope::err(error_codes::FILE_NOT_FOUND, "failed to read file")
    })
}

fn document_symbols(
    project_id: &str,
    project_root: &Path,
    supervisor: &LspSupervisor,
    relative_path: &str,
    limit: usize,
) -> Result<DocumentSymbolsResult, RunnerLspResultEnvelope> {
    let limit = limit.clamp(1, 500);
    let ResolvedSourceFile {
        path: file,
        profile,
        language_id,
    } = resolve_source_file(project_root, relative_path)?;
    let uri = file_uri(&file)?;
    let text = read_document_text(&file)?;
    // Take the encoding from prepare_document like goto/references do: the
    // post-request slot lookup could race a slot transition and silently fall
    // back to UTF-16 while the server negotiated another encoding.
    let encoding = supervisor
        .prepare_document(project_root, profile.kind, &uri, language_id, &text)
        .map_err(map_lsp_error)?;
    let value = supervisor
        .request_with_document(
            project_root,
            profile.kind,
            &uri,
            language_id,
            &text,
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": uri } }),
        )
        .map_err(map_lsp_error)?;
    let mut cache = LineCache::new();
    cache.seed(&file, text);
    let mut invalid = 0usize;
    let mut external = 0usize;
    let all = normalize_document_symbols(
        project_root,
        &file,
        &value,
        encoding,
        &mut cache,
        &mut invalid,
        &mut external,
    );
    let total_count = count_symbol_nodes(&all);
    let mut truncated = false;
    let symbols = take_symbol_budget(all, limit, &mut truncated);
    let returned_count = count_symbol_nodes(&symbols);
    Ok(DocumentSymbolsResult {
        project: project_id.to_string(),
        path: normalize_relative_path(relative_path),
        language: profile.language_id.to_string(),
        symbols,
        total_count,
        returned_count,
        truncated,
        external_results_omitted: external,
        invalid_results_omitted: invalid,
    })
}

fn document_diagnostics(
    project_id: &str,
    project_root: &Path,
    supervisor: &LspSupervisor,
    relative_path: &str,
    limit: usize,
) -> Result<DocumentDiagnosticsResult, RunnerLspResultEnvelope> {
    let limit = limit.clamp(1, 200);
    let ResolvedSourceFile {
        path: file,
        profile,
        language_id,
    } = resolve_source_file(project_root, relative_path)?;
    let uri = file_uri(&file)?;
    let text = read_document_text(&file)?;
    let deadline = Instant::now() + DIAGNOSTICS_WAIT_TIMEOUT;
    let snapshot = supervisor
        .document_diagnostics(
            project_root,
            profile.kind,
            &uri,
            language_id,
            &text,
            deadline,
        )
        .map_err(map_lsp_error)?;

    let mut invalid_results_omitted = 0usize;
    let related_information_omitted = snapshot
        .publication
        .as_ref()
        .map(|publication| publication.related_information_count)
        .unwrap_or(0);
    let mut cache = LineCache::new();
    cache.seed(&file, text);
    let mut diagnostics = snapshot
        .publication
        .as_ref()
        .map(|publication| {
            publication
                .diagnostics
                .iter()
                .filter_map(|value| {
                    match normalize_diagnostic(value, &file, snapshot.position_encoding, &mut cache)
                    {
                        Some(diagnostic) => Some(diagnostic),
                        None => {
                            invalid_results_omitted += 1;
                            None
                        }
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    diagnostics.sort_by(|left, right| diagnostic_sort_key(left).cmp(&diagnostic_sort_key(right)));
    diagnostics.dedup();
    let raw_count = snapshot
        .publication
        .as_ref()
        .map(|publication| publication.raw_diagnostics_count)
        .unwrap_or(0);
    let cached_count = snapshot
        .publication
        .as_ref()
        .map(|publication| publication.diagnostics.len())
        .unwrap_or(0);
    let mut text_chars = 0usize;
    let mut text_budget_count = diagnostics.len();
    for (index, diagnostic) in diagnostics.iter().enumerate() {
        let diagnostic_chars = diagnostic.message.chars().count()
            + diagnostic
                .source
                .as_deref()
                .map(|value| value.chars().count())
                .unwrap_or(0)
            + diagnostic
                .code
                .as_deref()
                .map(|value| value.chars().count())
                .unwrap_or(0);
        if text_chars.saturating_add(diagnostic_chars) > MAX_DIAGNOSTIC_TOTAL_TEXT_CHARS {
            text_budget_count = index;
            break;
        }
        text_chars += diagnostic_chars;
    }
    let truncated = raw_count > cached_count
        || diagnostics.len() > limit
        || text_budget_count < diagnostics.len();
    diagnostics.truncate(text_budget_count);
    diagnostics.truncate(limit);
    let status = if snapshot.timed_out {
        DocumentDiagnosticsStatus::Timeout
    } else {
        DocumentDiagnosticsStatus::Complete
    };
    let clean = (status == DocumentDiagnosticsStatus::Complete).then_some(raw_count == 0);

    Ok(DocumentDiagnosticsResult {
        project: project_id.to_string(),
        path: normalize_relative_path(relative_path),
        language: profile.language_id.to_string(),
        returned_count: diagnostics.len(),
        diagnostics,
        total_count: raw_count,
        truncated,
        status,
        clean,
        published_version: snapshot
            .publication
            .as_ref()
            .and_then(|publication| publication.version),
        invalid_results_omitted,
        related_information_omitted,
    })
}

fn hover(
    project_id: &str,
    project_root: &Path,
    supervisor: &LspSupervisor,
    relative_path: &str,
    line: usize,
    column: usize,
) -> Result<HoverResult, RunnerLspResultEnvelope> {
    let ResolvedSourceFile {
        path: file,
        profile,
        language_id,
    } = resolve_source_file(project_root, relative_path)?;
    let uri = file_uri(&file)?;
    let text = read_document_text(&file)?;
    let encoding = supervisor
        .prepare_document(project_root, profile.kind, &uri, language_id, &text)
        .map_err(map_lsp_error)?;
    let (lsp_line, lsp_character) = public_to_lsp(&text, line, column, encoding)
        .map_err(|message| RunnerLspResultEnvelope::err(error_codes::INVALID_ARGUMENTS, message))?;
    let value = supervisor
        .request_with_document(
            project_root,
            profile.kind,
            &uri,
            language_id,
            &text,
            "textDocument/hover",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": lsp_line, "character": lsp_character}
            }),
        )
        .map_err(map_lsp_error)?;
    let (hover, truncated, range_omitted) = normalize_hover(&value, &text, encoding)?;
    Ok(HoverResult {
        project: project_id.to_string(),
        path: normalize_relative_path(relative_path),
        position: PublicPosition { line, column },
        hover,
        truncated,
        range_omitted,
    })
}

fn workspace_symbols(
    project_id: &str,
    project_root: &Path,
    supervisor: &LspSupervisor,
    query: &str,
    limit: usize,
    operation_deadline: Instant,
) -> Result<WorkspaceSymbolsResult, RunnerLspResultEnvelope> {
    let query = query.trim();
    if query.is_empty() || query.chars().count() > 200 {
        return Err(RunnerLspResultEnvelope::err(
            error_codes::INVALID_ARGUMENTS,
            "query must contain 1..200 non-whitespace characters",
        ));
    }
    if redact_absolute_paths(query) != query {
        return Err(RunnerLspResultEnvelope::err(
            error_codes::INVALID_ARGUMENTS,
            "query must not contain absolute path material",
        ));
    }
    let limit = limit.clamp(1, 200);
    // Project-scoped query with no file path to route on: use the primary
    // detected language's server. Multi-server fan-out is a follow-up.
    let profile = primary_profile(project_root);
    let (value, encoding) = supervisor
        .request_workspace_with_position_encoding_until(
            project_root,
            profile.kind,
            "workspace/symbol",
            json!({"query": query}),
            operation_deadline,
        )
        .map_err(map_lsp_error)?;
    let (mut symbols, total_results, external_results_omitted, invalid_results_omitted) =
        normalize_workspace_symbols(project_root, profile, &value, encoding);
    symbols.sort_by(|left, right| {
        workspace_symbol_sort_key(left).cmp(&workspace_symbol_sort_key(right))
    });
    symbols.dedup();
    let truncated = symbols.len() > limit;
    symbols.truncate(limit);
    Ok(WorkspaceSymbolsResult {
        project: project_id.to_string(),
        query: query.to_string(),
        returned_count: symbols.len(),
        symbols,
        total_results,
        truncated,
        external_results_omitted,
        invalid_results_omitted,
    })
}

fn goto_definition(
    project_id: &str,
    project_root: &Path,
    supervisor: &LspSupervisor,
    relative_path: &str,
    line: usize,
    column: usize,
    limit: usize,
) -> Result<LocationsResult, RunnerLspResultEnvelope> {
    let limit = limit.clamp(1, 100);
    let ResolvedSourceFile {
        path: file,
        profile,
        language_id,
    } = resolve_source_file(project_root, relative_path)?;
    let uri = file_uri(&file)?;
    let text = read_document_text(&file)?;
    let encoding = supervisor
        .prepare_document(project_root, profile.kind, &uri, language_id, &text)
        .map_err(map_lsp_error)?;
    let (lsp_line, lsp_character) = public_to_lsp(&text, line, column, encoding)
        .map_err(|msg| RunnerLspResultEnvelope::err(error_codes::INVALID_ARGUMENTS, msg))?;
    let value = supervisor
        .request_with_document(
            project_root,
            profile.kind,
            &uri,
            language_id,
            &text,
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": lsp_line, "character": lsp_character }
            }),
        )
        .map_err(map_lsp_error)?;
    let mut cache = LineCache::new();
    cache.seed(&file, text);
    let (locations, total, external, invalid) =
        normalize_locations_value(project_root, &value, encoding, &mut cache);
    finish_locations_result(
        project_id,
        relative_path,
        line,
        column,
        locations,
        total,
        external,
        invalid,
        limit,
    )
}

fn find_references(
    project_id: &str,
    project_root: &Path,
    supervisor: &LspSupervisor,
    relative_path: &str,
    line: usize,
    column: usize,
    include_declaration: bool,
    limit: usize,
) -> Result<LocationsResult, RunnerLspResultEnvelope> {
    let limit = limit.clamp(1, 200);
    let ResolvedSourceFile {
        path: file,
        profile,
        language_id,
    } = resolve_source_file(project_root, relative_path)?;
    let uri = file_uri(&file)?;
    let text = read_document_text(&file)?;
    let encoding = supervisor
        .prepare_document(project_root, profile.kind, &uri, language_id, &text)
        .map_err(map_lsp_error)?;
    let (lsp_line, lsp_character) = public_to_lsp(&text, line, column, encoding)
        .map_err(|msg| RunnerLspResultEnvelope::err(error_codes::INVALID_ARGUMENTS, msg))?;
    let value = supervisor
        .request_with_document(
            project_root,
            profile.kind,
            &uri,
            language_id,
            &text,
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": lsp_line, "character": lsp_character },
                "context": { "includeDeclaration": include_declaration }
            }),
        )
        .map_err(map_lsp_error)?;
    let mut cache = LineCache::new();
    cache.seed(&file, text);
    let (locations, total, external, invalid) =
        normalize_locations_value(project_root, &value, encoding, &mut cache);
    finish_locations_result(
        project_id,
        relative_path,
        line,
        column,
        locations,
        total,
        external,
        invalid,
        limit,
    )
}

#[derive(Clone)]
struct NormalizedCallHierarchyItem {
    raw: Value,
    symbol: PublicCallHierarchySymbol,
    absolute_path: PathBuf,
}

struct CallHierarchyCandidate {
    direction: CallHierarchyEdgeDirection,
    depth: usize,
    from: PublicCallHierarchySymbol,
    to: PublicCallHierarchySymbol,
    target: NormalizedCallHierarchyItem,
    call_sites: Vec<PublicRange>,
    call_site_ranges_omitted: usize,
}

enum CallHierarchyItemNormalize {
    Ok(NormalizedCallHierarchyItem),
    External,
    Invalid,
}

#[allow(clippy::too_many_arguments)]
fn call_hierarchy(
    project_id: &str,
    project_root: &Path,
    supervisor: &LspSupervisor,
    relative_path: &str,
    line: usize,
    column: usize,
    direction: CallHierarchyDirection,
    depth: usize,
    limit: usize,
    operation_deadline: Instant,
) -> Result<CallHierarchyResult, RunnerLspResultEnvelope> {
    if line < 1 || column < 1 {
        return Err(RunnerLspResultEnvelope::err(
            error_codes::INVALID_ARGUMENTS,
            "line and column must be >= 1",
        ));
    }
    if let Err(message) = validate_call_hierarchy_bounds(depth, limit) {
        return Err(RunnerLspResultEnvelope::err(
            error_codes::INVALID_ARGUMENTS,
            message,
        ));
    }

    let ResolvedSourceFile {
        path: file,
        profile,
        language_id,
    } = resolve_source_file(project_root, relative_path)?;
    let uri = file_uri(&file)?;
    let text = read_document_text(&file)?;
    let query_path = project_relative_path(project_root, &file).ok_or_else(|| {
        RunnerLspResultEnvelope::err(
            error_codes::INVALID_PROJECT_PATH,
            "path resolves outside project root",
        )
    })?;
    // The prepare request synchronizes the document and returns the exact
    // encoding negotiated by the server instance that produced the items.
    ensure_call_hierarchy_budget(operation_deadline)?;
    let provisional_encoding = supervisor
        .prepare_document_until(
            project_root,
            profile.kind,
            &uri,
            language_id,
            &text,
            operation_deadline,
        )
        .map_err(map_lsp_error)?;
    let (lsp_line, lsp_character) = public_to_lsp(&text, line, column, provisional_encoding)
        .map_err(|message| RunnerLspResultEnvelope::err(error_codes::INVALID_ARGUMENTS, message))?;
    let (prepared, prepare_encoding) = supervisor
        .prepare_call_hierarchy(
            project_root,
            profile.kind,
            &uri,
            language_id,
            &text,
            lsp_line,
            lsp_character,
            operation_deadline,
        )
        .map_err(map_lsp_error)?;
    let prepared_items = call_hierarchy_array(&prepared, "prepareCallHierarchy")?;
    let root_total_count = prepared_items.len();
    let mut truncated = root_total_count > MAX_CALL_HIERARCHY_PREPARE_ITEMS_INSPECTED;
    let mut cache = LineCache::new();
    cache.seed(&file, text);
    let mut external_results_omitted = 0usize;
    let mut invalid_results_omitted = 0usize;
    let mut roots = Vec::new();
    for item in prepared_items
        .iter()
        .take(MAX_CALL_HIERARCHY_PREPARE_ITEMS_INSPECTED)
    {
        match normalize_call_hierarchy_item(project_root, item, prepare_encoding, &mut cache) {
            CallHierarchyItemNormalize::Ok(item) => roots.push(item),
            CallHierarchyItemNormalize::External => external_results_omitted += 1,
            CallHierarchyItemNormalize::Invalid => invalid_results_omitted += 1,
        }
    }
    roots.sort_by(|left, right| {
        call_hierarchy_symbol_key(&left.symbol)
            .cmp(&call_hierarchy_symbol_key(&right.symbol))
            .then_with(|| {
                serde_json::to_string(&left.raw)
                    .unwrap_or_default()
                    .cmp(&serde_json::to_string(&right.raw).unwrap_or_default())
            })
    });
    roots.dedup_by(|left, right| {
        call_hierarchy_symbol_key(&left.symbol) == call_hierarchy_symbol_key(&right.symbol)
    });

    truncated |= roots.len() > MAX_CALL_HIERARCHY_ROOTS;
    roots.truncate(MAX_CALL_HIERARCHY_ROOTS);
    let public_roots = roots
        .iter()
        .map(|item| item.symbol.clone())
        .collect::<Vec<_>>();
    let mut queue = VecDeque::new();
    let mut visited_symbols = HashSet::new();
    for root in roots {
        visited_symbols.insert(call_hierarchy_symbol_key(&root.symbol));
        queue.push_back((root, 0usize));
    }

    let mut edges = Vec::<PublicCallHierarchyEdge>::new();
    let mut edge_indexes = HashMap::<String, usize>::new();
    let mut call_site_ranges_omitted = 0usize;
    while let Some((current, current_depth)) = queue.pop_front() {
        if edges.len() >= limit {
            truncated = true;
            break;
        }
        if current_depth >= depth {
            continue;
        }
        let edge_depth = current_depth + 1;
        let mut candidates = Vec::new();
        if matches!(
            direction,
            CallHierarchyDirection::Incoming | CallHierarchyDirection::Both
        ) {
            ensure_call_hierarchy_budget(operation_deadline)?;
            let (value, encoding) = supervisor
                .incoming_call_hierarchy(
                    project_root,
                    profile.kind,
                    current.raw.clone(),
                    operation_deadline,
                )
                .map_err(map_lsp_error)?;
            normalize_call_hierarchy_calls(
                project_root,
                &current,
                &value,
                encoding,
                CallHierarchyEdgeDirection::Incoming,
                edge_depth,
                &mut cache,
                &mut candidates,
                &mut external_results_omitted,
                &mut invalid_results_omitted,
                &mut truncated,
            )?;
        }
        if matches!(
            direction,
            CallHierarchyDirection::Outgoing | CallHierarchyDirection::Both
        ) {
            ensure_call_hierarchy_budget(operation_deadline)?;
            let (value, encoding) = supervisor
                .outgoing_call_hierarchy(
                    project_root,
                    profile.kind,
                    current.raw.clone(),
                    operation_deadline,
                )
                .map_err(map_lsp_error)?;
            normalize_call_hierarchy_calls(
                project_root,
                &current,
                &value,
                encoding,
                CallHierarchyEdgeDirection::Outgoing,
                edge_depth,
                &mut cache,
                &mut candidates,
                &mut external_results_omitted,
                &mut invalid_results_omitted,
                &mut truncated,
            )?;
        }
        candidates.sort_by_key(call_hierarchy_candidate_key);

        let mut budget_exhausted = false;
        for candidate in candidates {
            let key = call_hierarchy_edge_key(
                candidate.direction,
                candidate.depth,
                &candidate.from,
                &candidate.to,
            );
            if let Some(index) = edge_indexes.get(&key).copied() {
                let omitted =
                    merge_call_site_ranges(&mut edges[index].call_sites, candidate.call_sites);
                call_site_ranges_omitted +=
                    candidate.call_site_ranges_omitted.saturating_add(omitted);
                continue;
            }
            if edges.len() >= limit {
                truncated = true;
                budget_exhausted = true;
                continue;
            }
            call_site_ranges_omitted += candidate.call_site_ranges_omitted;
            let target_key = call_hierarchy_symbol_key(&candidate.target.symbol);
            if edge_depth < depth && visited_symbols.insert(target_key) {
                queue.push_back((candidate.target.clone(), edge_depth));
            }
            let index = edges.len();
            edge_indexes.insert(key, index);
            edges.push(PublicCallHierarchyEdge {
                direction: candidate.direction,
                depth: candidate.depth,
                from: candidate.from,
                to: candidate.to,
                call_sites: candidate.call_sites,
            });
        }
        if budget_exhausted {
            break;
        }
    }
    edges.sort_by_key(call_hierarchy_public_edge_key);
    if call_site_ranges_omitted > 0 {
        truncated = true;
    }
    let root_returned_count = public_roots.len();

    Ok(CallHierarchyResult {
        project: project_id.to_string(),
        path: query_path,
        language: profile.language_id.to_string(),
        query_position: PublicPosition { line, column },
        direction,
        depth,
        roots: public_roots,
        root_total_count,
        root_returned_count,
        returned_count: edges.len(),
        edges,
        truncated,
        external_results_omitted,
        invalid_results_omitted,
        call_site_ranges_omitted,
    })
}

fn ensure_call_hierarchy_budget(
    operation_deadline: Instant,
) -> Result<(), RunnerLspResultEnvelope> {
    if Instant::now() >= operation_deadline {
        return Err(RunnerLspResultEnvelope::err(
            error_codes::LSP_REQUEST_TIMEOUT,
            "language server request timed out",
        ));
    }
    Ok(())
}

fn call_hierarchy_array<'a>(
    value: &'a Value,
    operation: &str,
) -> Result<&'a [Value], RunnerLspResultEnvelope> {
    if value.is_null() {
        return Ok(&[]);
    }
    value.as_array().map(Vec::as_slice).ok_or_else(|| {
        RunnerLspResultEnvelope::err(
            error_codes::LSP_PROTOCOL_ERROR,
            format!("malformed {operation} result"),
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn normalize_call_hierarchy_calls(
    project_root: &Path,
    current: &NormalizedCallHierarchyItem,
    value: &Value,
    encoding: PositionEncoding,
    direction: CallHierarchyEdgeDirection,
    depth: usize,
    cache: &mut LineCache,
    candidates: &mut Vec<CallHierarchyCandidate>,
    external_results_omitted: &mut usize,
    invalid_results_omitted: &mut usize,
    truncated: &mut bool,
) -> Result<(), RunnerLspResultEnvelope> {
    let operation = match direction {
        CallHierarchyEdgeDirection::Incoming => "callHierarchy/incomingCalls",
        CallHierarchyEdgeDirection::Outgoing => "callHierarchy/outgoingCalls",
    };
    let calls = call_hierarchy_array(value, operation)?;
    if calls.len() > MAX_CALL_HIERARCHY_CALL_ENTRIES_INSPECTED_PER_RPC {
        *truncated = true;
    }
    for call in calls
        .iter()
        .take(MAX_CALL_HIERARCHY_CALL_ENTRIES_INSPECTED_PER_RPC)
    {
        let Some(call) = call.as_object() else {
            *invalid_results_omitted += 1;
            continue;
        };
        let item_field = match direction {
            CallHierarchyEdgeDirection::Incoming => "from",
            CallHierarchyEdgeDirection::Outgoing => "to",
        };
        let Some(raw_target) = call.get(item_field) else {
            *invalid_results_omitted += 1;
            continue;
        };
        let target = match normalize_call_hierarchy_item(project_root, raw_target, encoding, cache)
        {
            CallHierarchyItemNormalize::Ok(item) => item,
            CallHierarchyItemNormalize::External => {
                *external_results_omitted += 1;
                continue;
            }
            CallHierarchyItemNormalize::Invalid => {
                *invalid_results_omitted += 1;
                continue;
            }
        };
        let Some(raw_ranges) = call.get("fromRanges").and_then(Value::as_array) else {
            *invalid_results_omitted += 1;
            continue;
        };
        let call_site_path = match direction {
            CallHierarchyEdgeDirection::Incoming => &target.absolute_path,
            CallHierarchyEdgeDirection::Outgoing => &current.absolute_path,
        };
        let mut call_sites = Vec::new();
        let raw_call_site_ranges_omitted = raw_ranges
            .len()
            .saturating_sub(MAX_CALL_HIERARCHY_RAW_CALL_SITE_RANGES_INSPECTED_PER_ENTRY);
        for raw_range in raw_ranges
            .iter()
            .take(MAX_CALL_HIERARCHY_RAW_CALL_SITE_RANGES_INSPECTED_PER_ENTRY)
        {
            match convert_range(cache, call_site_path, raw_range, encoding) {
                Some(range) => call_sites.push(range),
                None => *invalid_results_omitted += 1,
            }
        }
        call_sites.sort_by_key(call_hierarchy_range_key);
        call_sites.dedup();
        let call_site_ranges_omitted = raw_call_site_ranges_omitted.saturating_add(
            call_sites
                .len()
                .saturating_sub(MAX_CALL_HIERARCHY_CALL_SITES_PER_EDGE),
        );
        call_sites.truncate(MAX_CALL_HIERARCHY_CALL_SITES_PER_EDGE);
        let (from, to) = match direction {
            CallHierarchyEdgeDirection::Incoming => (target.symbol.clone(), current.symbol.clone()),
            CallHierarchyEdgeDirection::Outgoing => (current.symbol.clone(), target.symbol.clone()),
        };
        candidates.push(CallHierarchyCandidate {
            direction,
            depth,
            from,
            to,
            target,
            call_sites,
            call_site_ranges_omitted,
        });
    }
    Ok(())
}

fn normalize_call_hierarchy_item(
    project_root: &Path,
    value: &Value,
    encoding: PositionEncoding,
    cache: &mut LineCache,
) -> CallHierarchyItemNormalize {
    let Some(object) = value.as_object() else {
        return CallHierarchyItemNormalize::Invalid;
    };
    let Some(name) = object.get("name").and_then(Value::as_str) else {
        return CallHierarchyItemNormalize::Invalid;
    };
    let name = bound_call_hierarchy_name(name);
    if name.is_empty() {
        return CallHierarchyItemNormalize::Invalid;
    }
    let Some(kind_code) = object.get("kind").and_then(Value::as_i64) else {
        return CallHierarchyItemNormalize::Invalid;
    };
    let Some(uri) = object.get("uri").and_then(Value::as_str) else {
        return CallHierarchyItemNormalize::Invalid;
    };
    let absolute_path = match classify_uri_against_project_root(project_root, uri) {
        ProjectUriClassification::InsideProject(path) => path,
        ProjectUriClassification::OutsideProject => return CallHierarchyItemNormalize::External,
        ProjectUriClassification::Unsupported => return CallHierarchyItemNormalize::Invalid,
    };
    let Some(path) = project_relative_path(project_root, &absolute_path) else {
        return CallHierarchyItemNormalize::External;
    };
    let Some(range) = object
        .get("range")
        .and_then(|range| convert_range(cache, &absolute_path, range, encoding))
    else {
        return CallHierarchyItemNormalize::Invalid;
    };
    let Some(selection_range) = object
        .get("selectionRange")
        .and_then(|range| convert_range(cache, &absolute_path, range, encoding))
    else {
        return CallHierarchyItemNormalize::Invalid;
    };
    CallHierarchyItemNormalize::Ok(NormalizedCallHierarchyItem {
        raw: value.clone(),
        symbol: PublicCallHierarchySymbol {
            name,
            kind: symbol_kind_name(kind_code).to_string(),
            kind_code,
            path,
            range,
            selection_range,
        },
        absolute_path,
    })
}

fn bound_call_hierarchy_name(name: &str) -> String {
    let sanitized = redact_absolute_paths(name)
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let sanitized = sanitized.trim();
    if sanitized.chars().count() <= MAX_SYMBOL_NAME_CHARS {
        return sanitized.to_string();
    }
    sanitized
        .chars()
        .take(MAX_SYMBOL_NAME_CHARS.saturating_sub(1))
        .collect::<String>()
        + "…"
}

fn call_hierarchy_range_key(range: &PublicRange) -> (usize, usize, usize, usize) {
    (
        range.start.line,
        range.start.column,
        range.end.line,
        range.end.column,
    )
}

fn call_hierarchy_symbol_key(symbol: &PublicCallHierarchySymbol) -> String {
    serde_json::to_string(symbol).unwrap_or_default()
}

fn call_hierarchy_edge_key(
    direction: CallHierarchyEdgeDirection,
    depth: usize,
    from: &PublicCallHierarchySymbol,
    to: &PublicCallHierarchySymbol,
) -> String {
    format!(
        "{direction:?}:{depth}:{}:{}",
        call_hierarchy_symbol_key(from),
        call_hierarchy_symbol_key(to)
    )
}

fn call_hierarchy_candidate_key(candidate: &CallHierarchyCandidate) -> String {
    format!(
        "{}:{}",
        call_hierarchy_edge_key(
            candidate.direction,
            candidate.depth,
            &candidate.from,
            &candidate.to,
        ),
        serde_json::to_string(&candidate.target.raw).unwrap_or_default()
    )
}

fn call_hierarchy_public_edge_key(edge: &PublicCallHierarchyEdge) -> String {
    call_hierarchy_edge_key(edge.direction, edge.depth, &edge.from, &edge.to)
}

fn merge_call_site_ranges(existing: &mut Vec<PublicRange>, additional: Vec<PublicRange>) -> usize {
    existing.extend(additional);
    existing.sort_by_key(call_hierarchy_range_key);
    existing.dedup();
    let omitted = existing
        .len()
        .saturating_sub(MAX_CALL_HIERARCHY_CALL_SITES_PER_EDGE);
    existing.truncate(MAX_CALL_HIERARCHY_CALL_SITES_PER_EDGE);
    omitted
}

fn finish_locations_result(
    project_id: &str,
    relative_path: &str,
    line: usize,
    column: usize,
    mut locations: Vec<PublicLocation>,
    total_results: usize,
    external_results_omitted: usize,
    invalid_results_omitted: usize,
    limit: usize,
) -> Result<LocationsResult, RunnerLspResultEnvelope> {
    locations.sort_by(|a, b| {
        (
            a.path.as_str(),
            a.range.start.line,
            a.range.start.column,
            a.range.end.line,
            a.range.end.column,
        )
            .cmp(&(
                b.path.as_str(),
                b.range.start.line,
                b.range.start.column,
                b.range.end.line,
                b.range.end.column,
            ))
    });
    locations.dedup();
    let project_valid = locations.len();
    let truncated = project_valid > limit;
    locations.truncate(limit);
    Ok(LocationsResult {
        project: project_id.to_string(),
        path: normalize_relative_path(relative_path),
        query_position: PublicPosition { line, column },
        returned_count: locations.len(),
        locations,
        total_results,
        truncated,
        external_results_omitted,
        invalid_results_omitted,
    })
}

/// A validated source file with the language profile that owns its extension
/// and the LSP `languageId` to announce for that extension.
struct ResolvedSourceFile {
    path: PathBuf,
    profile: &'static LanguageProfile,
    /// `languageId` for `textDocument/didOpen`; may be a dialect id such as
    /// `typescriptreact` distinct from `profile.language_id`.
    language_id: &'static str,
}

/// Validate a project-relative source path and route it to the language
/// profile that owns its extension.
fn resolve_source_file(
    project_root: &Path,
    relative_path: &str,
) -> Result<ResolvedSourceFile, RunnerLspResultEnvelope> {
    let path = relative_path.trim();
    if path.is_empty() {
        return Err(RunnerLspResultEnvelope::err(
            error_codes::INVALID_PROJECT_PATH,
            "path cannot be empty",
        ));
    }
    let raw = Path::new(path);
    // Windows-only subtlety: `Path::is_absolute` is false for root-relative
    // inputs (`/etc/...`, `\etc\...`) and `Path::has_root` is false for
    // drive-relative inputs (`C:file.rs`); none of these are project-relative
    // paths. Checking the leading component for a root or drive/UNC prefix
    // rejects every such form uniformly on both platforms.
    if raw.is_absolute()
        || raw.components().next().is_some_and(|component| {
            matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(RunnerLspResultEnvelope::err(
            error_codes::INVALID_PROJECT_PATH,
            "path must be project-relative",
        ));
    }
    if raw
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(RunnerLspResultEnvelope::err(
            error_codes::INVALID_PROJECT_PATH,
            "path must not contain '..'",
        ));
    }
    let (profile, language_id) = raw
        .extension()
        .and_then(|ext| ext.to_str())
        .and_then(route_extension)
        .ok_or_else(|| {
            RunnerLspResultEnvelope::err(
                error_codes::UNSUPPORTED_LANGUAGE,
                format!(
                    "unsupported file extension for LSP navigation (supported: {})",
                    supported_extensions_label()
                ),
            )
        })?;
    let joined = project_root.join(raw);
    let canonical = fs::canonicalize(&joined)
        .map_err(|_| RunnerLspResultEnvelope::err(error_codes::FILE_NOT_FOUND, "file not found"))?;
    if !canonical.starts_with(project_root) {
        return Err(RunnerLspResultEnvelope::err(
            error_codes::INVALID_PROJECT_PATH,
            "path resolves outside project root",
        ));
    }
    if !canonical.is_file() {
        return Err(RunnerLspResultEnvelope::err(
            error_codes::FILE_NOT_FOUND,
            "path is not a regular file",
        ));
    }
    Ok(ResolvedSourceFile {
        path: canonical,
        profile,
        language_id,
    })
}

fn file_uri(path: &Path) -> Result<String, RunnerLspResultEnvelope> {
    Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|_| {
            RunnerLspResultEnvelope::err(error_codes::INVALID_PROJECT_PATH, "invalid file path")
        })
}

fn normalize_relative_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn project_relative_path(project_root: &Path, absolute: &Path) -> Option<String> {
    let rel = absolute.strip_prefix(project_root).ok()?;
    let text = rel.to_str()?;
    Some(text.replace('\\', "/"))
}

fn normalize_hover(
    value: &Value,
    text: &str,
    encoding: PositionEncoding,
) -> Result<(Option<PublicHover>, bool, bool), RunnerLspResultEnvelope> {
    if value.is_null() {
        return Ok((None, false, false));
    }
    let object = value.as_object().ok_or_else(|| {
        RunnerLspResultEnvelope::err(error_codes::LSP_PROTOCOL_ERROR, "malformed hover result")
    })?;
    let contents = object.get("contents").ok_or_else(|| {
        RunnerLspResultEnvelope::err(
            error_codes::LSP_PROTOCOL_ERROR,
            "hover result is missing contents",
        )
    })?;
    let (kind, raw_value) = normalize_hover_contents(contents).ok_or_else(|| {
        RunnerLspResultEnvelope::err(
            error_codes::LSP_PROTOCOL_ERROR,
            "hover contents use an unsupported shape",
        )
    })?;
    let (value, truncated) = bound_hover_value(&raw_value);
    let (range, range_omitted) = match object.get("range") {
        Some(range) => match convert_range_from_text(text, range, encoding) {
            Some(range) => (Some(range), false),
            None => (None, true),
        },
        None => (None, false),
    };
    Ok((
        Some(PublicHover { kind, value, range }),
        truncated,
        range_omitted,
    ))
}

fn normalize_hover_contents(contents: &Value) -> Option<(String, String)> {
    match contents {
        Value::String(value) => Some(("markdown".to_string(), value.clone())),
        Value::Object(object) => {
            let value = object.get("value")?.as_str()?;
            if let Some(kind) = object.get("kind") {
                let kind = kind.as_str()?;
                if !matches!(kind, "markdown" | "plaintext") {
                    return None;
                }
                return Some((kind.to_string(), value.to_string()));
            }
            let language = object.get("language")?.as_str()?;
            Some((
                "markdown".to_string(),
                fenced_marked_string(language, value),
            ))
        }
        Value::Array(items) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(value) => values.push(value.clone()),
                    Value::Object(object) => {
                        let language = object.get("language")?.as_str()?;
                        let value = object.get("value")?.as_str()?;
                        values.push(fenced_marked_string(language, value));
                    }
                    _ => return None,
                }
            }
            Some(("markdown".to_string(), values.join("\n\n")))
        }
        _ => None,
    }
}

fn fenced_marked_string(language: &str, value: &str) -> String {
    let language = language
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '+' | '#')
        })
        .take(32)
        .collect::<String>();
    let language = if language.is_empty() {
        "text"
    } else {
        &language
    };
    let mut current_run = 0usize;
    let mut longest_run = 0usize;
    for character in value.chars() {
        if character == '`' {
            current_run += 1;
            longest_run = longest_run.max(current_run);
        } else {
            current_run = 0;
        }
    }
    let fence = "`".repeat(longest_run.saturating_add(1).max(3));
    format!("{fence}{language}\n{value}\n{fence}")
}

fn bound_hover_value(value: &str) -> (String, bool) {
    let sanitized = redact_absolute_paths(value)
        .chars()
        .map(|character| match character {
            '\n' | '\t' => character,
            '\r' => '\n',
            character if character.is_control() => ' ',
            character => character,
        })
        .collect::<String>();
    if sanitized.chars().count() <= MAX_HOVER_VALUE_CHARS {
        return (sanitized, false);
    }
    (
        sanitized
            .chars()
            .take(MAX_HOVER_VALUE_CHARS.saturating_sub(1))
            .collect::<String>()
            + "…",
        true,
    )
}

fn normalize_workspace_symbols(
    project_root: &Path,
    profile: &LanguageProfile,
    value: &Value,
    encoding: PositionEncoding,
) -> (Vec<PublicWorkspaceSymbol>, usize, usize, usize) {
    if value.is_null() {
        return (Vec::new(), 0, 0, 0);
    }
    let Some(items) = value.as_array() else {
        return (Vec::new(), 1, 0, 1);
    };
    let mut symbols = Vec::new();
    let mut external = 0usize;
    let mut invalid = 0usize;
    let mut cache = LineCache::new();
    for item in items {
        match normalize_workspace_symbol(project_root, profile, item, encoding, &mut cache) {
            WorkspaceSymbolNormalize::Ok(symbol) => symbols.push(symbol),
            WorkspaceSymbolNormalize::External => external += 1,
            WorkspaceSymbolNormalize::Invalid => invalid += 1,
        }
    }
    (symbols, items.len(), external, invalid)
}

enum WorkspaceSymbolNormalize {
    Ok(PublicWorkspaceSymbol),
    External,
    Invalid,
}

fn normalize_workspace_symbol(
    project_root: &Path,
    profile: &LanguageProfile,
    value: &Value,
    encoding: PositionEncoding,
    cache: &mut LineCache,
) -> WorkspaceSymbolNormalize {
    let Some(object) = value.as_object() else {
        return WorkspaceSymbolNormalize::Invalid;
    };
    let Some(name) = object.get("name").and_then(Value::as_str) else {
        return WorkspaceSymbolNormalize::Invalid;
    };
    let name = bound_diagnostic_field(name, MAX_WORKSPACE_SYMBOL_FIELD_CHARS);
    if name.is_empty() {
        return WorkspaceSymbolNormalize::Invalid;
    }
    let Some(kind_code) = object.get("kind").and_then(Value::as_i64) else {
        return WorkspaceSymbolNormalize::Invalid;
    };
    let Some(location) = object.get("location").and_then(Value::as_object) else {
        return WorkspaceSymbolNormalize::Invalid;
    };
    let Some(uri) = location.get("uri").and_then(Value::as_str) else {
        return WorkspaceSymbolNormalize::Invalid;
    };
    let path = match classify_uri_against_project_root(project_root, uri) {
        ProjectUriClassification::InsideProject(path) => path,
        ProjectUriClassification::OutsideProject => return WorkspaceSymbolNormalize::External,
        ProjectUriClassification::Unsupported => return WorkspaceSymbolNormalize::Invalid,
    };
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_none_or(|extension| !profile.handles_extension(extension))
    {
        return WorkspaceSymbolNormalize::Invalid;
    }
    let Some(relative_path) = project_relative_path(project_root, &path) else {
        return WorkspaceSymbolNormalize::External;
    };
    let range = match location.get("range") {
        Some(range) => match convert_range(cache, &path, range, encoding) {
            Some(range) => Some(range),
            None => return WorkspaceSymbolNormalize::Invalid,
        },
        None => None,
    };
    let container_name = object
        .get("containerName")
        .and_then(Value::as_str)
        .map(|value| bound_diagnostic_field(value, MAX_WORKSPACE_SYMBOL_FIELD_CHARS))
        .filter(|value| !value.is_empty());
    WorkspaceSymbolNormalize::Ok(PublicWorkspaceSymbol {
        name,
        kind: symbol_kind_name(kind_code).to_string(),
        kind_code,
        container_name,
        path: relative_path,
        range,
    })
}

fn workspace_symbol_sort_key(
    symbol: &PublicWorkspaceSymbol,
) -> (
    &str,
    &str,
    &str,
    Option<(usize, usize, usize, usize)>,
    Option<&str>,
) {
    (
        symbol.name.as_str(),
        symbol.kind.as_str(),
        symbol.path.as_str(),
        symbol.range.as_ref().map(|range| {
            (
                range.start.line,
                range.start.column,
                range.end.line,
                range.end.column,
            )
        }),
        symbol.container_name.as_deref(),
    )
}

fn normalize_diagnostic(
    value: &Value,
    document_path: &Path,
    encoding: PositionEncoding,
    cache: &mut LineCache,
) -> Option<PublicDiagnostic> {
    let value = value.as_object()?;
    let range = convert_range(cache, document_path, value.get("range")?, encoding)?;
    let message = value.get("message")?.as_str()?;
    let severity_code = value.get("severity").and_then(Value::as_i64);
    let severity = match severity_code {
        Some(1) => "error",
        Some(2) => "warning",
        Some(3) => "information",
        Some(4) => "hint",
        _ => "unknown",
    }
    .to_string();
    let code = value.get("code").and_then(|code| match code {
        Value::String(code) => Some(bound_diagnostic_field(code, MAX_DIAGNOSTIC_CODE_CHARS)),
        Value::Number(code) => Some(code.to_string()),
        _ => None,
    });
    let source = value
        .get("source")
        .and_then(Value::as_str)
        .map(|source| bound_diagnostic_field(source, MAX_DIAGNOSTIC_SOURCE_CHARS))
        .filter(|source| !source.is_empty());
    let mut unnecessary = false;
    let mut deprecated = false;
    let mut unknown = false;
    if let Some(tags) = value.get("tags") {
        if let Some(tags) = tags.as_array() {
            for tag in tags {
                match tag.as_i64() {
                    Some(1) => unnecessary = true,
                    Some(2) => deprecated = true,
                    _ => unknown = true,
                }
            }
        } else {
            unknown = true;
        }
    }
    let mut tags = Vec::new();
    if unnecessary {
        tags.push("unnecessary".to_string());
    }
    if deprecated {
        tags.push("deprecated".to_string());
    }
    if unknown {
        tags.push("unknown".to_string());
    }
    Some(PublicDiagnostic {
        range,
        severity,
        severity_code,
        code,
        source,
        message: bound_diagnostic_field(message, MAX_DIAGNOSTIC_MESSAGE_CHARS),
        tags,
    })
}

fn diagnostic_sort_key(
    diagnostic: &PublicDiagnostic,
) -> (u8, usize, usize, usize, usize, Option<&str>, &str) {
    let severity = match diagnostic.severity.as_str() {
        "error" => 0,
        "warning" => 1,
        "information" => 2,
        "hint" => 3,
        _ => 4,
    };
    (
        severity,
        diagnostic.range.start.line,
        diagnostic.range.start.column,
        diagnostic.range.end.line,
        diagnostic.range.end.column,
        diagnostic.code.as_deref(),
        diagnostic.message.as_str(),
    )
}

fn bound_diagnostic_field(value: &str, max_chars: usize) -> String {
    let sanitized = redact_absolute_paths(value)
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let sanitized = sanitized.trim();
    if sanitized.chars().count() <= max_chars {
        return sanitized.to_string();
    }
    sanitized
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>()
        + "…"
}

fn normalize_locations_value(
    project_root: &Path,
    value: &Value,
    encoding: PositionEncoding,
    cache: &mut LineCache,
) -> (Vec<PublicLocation>, usize, usize, usize) {
    let mut locations = Vec::new();
    let mut external = 0usize;
    let mut invalid = 0usize;
    let mut total = 0usize;
    if value.is_null() {
        return (locations, 0, 0, 0);
    }
    if let Some(array) = value.as_array() {
        for item in array {
            total += 1;
            match normalize_location_or_link(project_root, item, encoding, cache) {
                LocationNormalize::Ok(loc) => locations.push(loc),
                LocationNormalize::External => external += 1,
                LocationNormalize::Invalid => invalid += 1,
            }
        }
        return (locations, total, external, invalid);
    }
    total = 1;
    match normalize_location_or_link(project_root, value, encoding, cache) {
        LocationNormalize::Ok(loc) => locations.push(loc),
        LocationNormalize::External => external += 1,
        LocationNormalize::Invalid => invalid += 1,
    }
    (locations, total, external, invalid)
}

enum LocationNormalize {
    Ok(PublicLocation),
    External,
    Invalid,
}

fn normalize_location_or_link(
    project_root: &Path,
    value: &Value,
    encoding: PositionEncoding,
    cache: &mut LineCache,
) -> LocationNormalize {
    if value.get("targetUri").is_some() {
        // LocationLink
        let Some(uri) = value.get("targetUri").and_then(Value::as_str) else {
            return LocationNormalize::Invalid;
        };
        let path = match classify_uri_against_project_root(project_root, uri) {
            ProjectUriClassification::InsideProject(path) => path,
            ProjectUriClassification::OutsideProject => return LocationNormalize::External,
            ProjectUriClassification::Unsupported => return LocationNormalize::Invalid,
        };
        let Some(rel) = project_relative_path(project_root, &path) else {
            return LocationNormalize::External;
        };
        let Some(selection) = value.get("targetSelectionRange") else {
            return LocationNormalize::Invalid;
        };
        let Some(range) = convert_range(cache, &path, selection, encoding) else {
            return LocationNormalize::Invalid;
        };
        let target_range = value
            .get("targetRange")
            .and_then(|range| convert_range(cache, &path, range, encoding));
        return LocationNormalize::Ok(PublicLocation {
            path: rel,
            range,
            target_range,
        });
    }
    let Some(uri) = value.get("uri").and_then(Value::as_str) else {
        return LocationNormalize::Invalid;
    };
    let path = match classify_uri_against_project_root(project_root, uri) {
        ProjectUriClassification::InsideProject(path) => path,
        ProjectUriClassification::OutsideProject => return LocationNormalize::External,
        ProjectUriClassification::Unsupported => return LocationNormalize::Invalid,
    };
    let Some(rel) = project_relative_path(project_root, &path) else {
        return LocationNormalize::External;
    };
    let Some(range_value) = value.get("range") else {
        return LocationNormalize::Invalid;
    };
    let Some(range) = convert_range(cache, &path, range_value, encoding) else {
        return LocationNormalize::Invalid;
    };
    LocationNormalize::Ok(PublicLocation {
        path: rel,
        range,
        target_range: None,
    })
}

fn convert_range(
    cache: &mut LineCache,
    path: &Path,
    range: &Value,
    encoding: PositionEncoding,
) -> Option<PublicRange> {
    let text = cache.text(path)?;
    convert_range_from_text(text, range, encoding)
}

fn convert_range_from_text(
    text: &str,
    range: &Value,
    encoding: PositionEncoding,
) -> Option<PublicRange> {
    let start = range.get("start")?;
    let end = range.get("end")?;
    let start_line = u32::try_from(start.get("line")?.as_u64()?).ok()?;
    let start_character = u32::try_from(start.get("character")?.as_u64()?).ok()?;
    let end_line = u32::try_from(end.get("line")?.as_u64()?).ok()?;
    let end_character = u32::try_from(end.get("character")?.as_u64()?).ok()?;
    let (sl, sc) = lsp_to_public(text, start_line, start_character, encoding)?;
    let (el, ec) = lsp_to_public(text, end_line, end_character, encoding)?;
    let range = PublicRange {
        start: PublicPosition {
            line: sl,
            column: sc,
        },
        end: PublicPosition {
            line: el,
            column: ec,
        },
    };
    if (range.end.line, range.end.column) < (range.start.line, range.start.column) {
        return None;
    }
    Some(range)
}

fn normalize_document_symbols(
    project_root: &Path,
    document_path: &Path,
    value: &Value,
    encoding: PositionEncoding,
    cache: &mut LineCache,
    invalid: &mut usize,
    external: &mut usize,
) -> Vec<PublicSymbol> {
    let Some(array) = value.as_array() else {
        if !value.is_null() {
            *invalid += 1;
        }
        return Vec::new();
    };
    // Detect SymbolInformation[] (has location) vs DocumentSymbol[] (has range).
    if array
        .first()
        .and_then(|item| item.get("location"))
        .is_some()
    {
        let mut symbols = Vec::new();
        for item in array {
            match normalize_symbol_information(project_root, item, encoding, cache) {
                Some(symbol) => symbols.push(symbol),
                None => {
                    // Distinguish external URI vs invalid.
                    if item
                        .pointer("/location/uri")
                        .and_then(Value::as_str)
                        .is_some_and(|uri| {
                            matches!(
                                classify_uri_against_project_root(project_root, uri),
                                ProjectUriClassification::OutsideProject
                            )
                        })
                    {
                        *external += 1;
                    } else {
                        *invalid += 1;
                    }
                }
            }
        }
        return symbols;
    }
    let mut symbols = Vec::new();
    for item in array {
        if let Some(symbol) =
            normalize_document_symbol_node(document_path, item, encoding, cache, invalid)
        {
            symbols.push(symbol);
        }
    }
    symbols
}

fn normalize_document_symbol_node(
    document_path: &Path,
    value: &Value,
    encoding: PositionEncoding,
    cache: &mut LineCache,
    invalid: &mut usize,
) -> Option<PublicSymbol> {
    let name = value.get("name").and_then(Value::as_str).unwrap_or("");
    if name.is_empty() {
        *invalid += 1;
        return None;
    }
    let kind_code = value.get("kind").and_then(Value::as_i64).unwrap_or(0);
    let range = match value
        .get("range")
        .and_then(|range| convert_range(cache, document_path, range, encoding))
    {
        Some(range) => range,
        None => {
            *invalid += 1;
            return None;
        }
    };
    let selection_range = value
        .get("selectionRange")
        .and_then(|range| convert_range(cache, document_path, range, encoding))
        .unwrap_or_else(|| range.clone());
    let detail = value
        .get("detail")
        .and_then(Value::as_str)
        .map(bound_symbol_detail);
    let mut children = Vec::new();
    if let Some(child_values) = value.get("children").and_then(Value::as_array) {
        for child in child_values {
            if let Some(symbol) =
                normalize_document_symbol_node(document_path, child, encoding, cache, invalid)
            {
                children.push(symbol);
            }
        }
    }
    Some(PublicSymbol {
        name: bound_symbol_name(name),
        kind: symbol_kind_name(kind_code).to_string(),
        kind_code,
        detail,
        range,
        selection_range,
        children,
    })
}

fn normalize_symbol_information(
    project_root: &Path,
    value: &Value,
    encoding: PositionEncoding,
    cache: &mut LineCache,
) -> Option<PublicSymbol> {
    let name = value.get("name").and_then(Value::as_str)?;
    let kind_code = value.get("kind").and_then(Value::as_i64).unwrap_or(0);
    let location = value.get("location")?;
    match normalize_location_or_link(project_root, location, encoding, cache) {
        LocationNormalize::Ok(loc) => Some(PublicSymbol {
            name: bound_symbol_name(name),
            kind: symbol_kind_name(kind_code).to_string(),
            kind_code,
            detail: None,
            range: loc.range.clone(),
            selection_range: loc.range,
            children: Vec::new(),
        }),
        _ => None,
    }
}

fn bound_symbol_name(name: &str) -> String {
    bound_symbol_field(name, MAX_SYMBOL_NAME_CHARS)
}

fn bound_symbol_detail(detail: &str) -> String {
    bound_symbol_field(detail, MAX_SYMBOL_DETAIL_CHARS)
}

fn bound_symbol_field(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>()
        + "…"
}

/// Map LSP SymbolKind integer to a stable lowercase name.
fn symbol_kind_name(kind_code: i64) -> &'static str {
    match kind_code {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum_member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type_parameter",
        _ => "unknown",
    }
}

fn count_symbol_nodes(symbols: &[PublicSymbol]) -> usize {
    symbols
        .iter()
        .map(|symbol| 1 + count_symbol_nodes(&symbol.children))
        .sum()
}

fn take_symbol_budget(
    symbols: Vec<PublicSymbol>,
    budget: usize,
    truncated: &mut bool,
) -> Vec<PublicSymbol> {
    let mut remaining = budget;
    let mut out = Vec::new();
    for symbol in symbols {
        if remaining == 0 {
            *truncated = true;
            break;
        }
        remaining -= 1;
        let mut children_truncated = false;
        let children = take_symbol_budget(symbol.children, remaining, &mut children_truncated);
        let used = count_symbol_nodes(&children);
        remaining = remaining.saturating_sub(used);
        if children_truncated {
            *truncated = true;
        }
        out.push(PublicSymbol {
            name: symbol.name,
            kind: symbol.kind,
            kind_code: symbol.kind_code,
            detail: symbol.detail,
            range: symbol.range,
            selection_range: symbol.selection_range,
            children,
        });
        if remaining == 0 && children_truncated {
            break;
        }
    }
    out
}

fn map_lsp_error(error: LspError) -> RunnerLspResultEnvelope {
    let (code, message) = match &error {
        LspError::ServerUnavailable => (
            error_codes::LSP_SERVER_UNAVAILABLE,
            "language server is unavailable".to_string(),
        ),
        LspError::RequestTimeout { .. } => (
            error_codes::LSP_REQUEST_TIMEOUT,
            "language server request timed out".to_string(),
        ),
        LspError::JsonRpc { message, .. } | LspError::ProtocolError(message) => (
            error_codes::LSP_PROTOCOL_ERROR,
            bound_error_message(message),
        ),
        LspError::RestartExhausted(message) => (
            error_codes::LSP_SERVER_FAILED,
            bound_error_message(format!("restart exhausted: {message}")),
        ),
        LspError::ServerExited => (
            error_codes::LSP_SERVER_FAILED,
            "language server exited".to_string(),
        ),
        LspError::SpawnFailed(message) => (
            error_codes::LSP_SERVER_FAILED,
            bound_error_message(format!("failed to spawn language server: {message}")),
        ),
        LspError::InitializeFailed(message) => (
            error_codes::LSP_SERVER_FAILED,
            bound_error_message(format!("language server initialize failed: {message}")),
        ),
        LspError::InvalidProjectRoot(_) => (
            error_codes::INVALID_PROJECT_PATH,
            "invalid project root".to_string(),
        ),
        LspError::CallHierarchyUnsupported => (
            error_codes::CALL_HIERARCHY_UNSUPPORTED,
            "language server does not support call hierarchy".to_string(),
        ),
        other => (
            error_codes::LSP_SERVER_FAILED,
            bound_error_message(other.to_string()),
        ),
    };
    RunnerLspResultEnvelope::err(code, sanitize_path_message(message))
}

fn sanitize_path_message(message: impl Into<String>) -> String {
    // `bound_error_message` redacts file URIs, absolute POSIX/Windows paths
    // (including quoted and `key=/...` embedded forms), scrubs control
    // characters, and truncates. Kept as a named wrapper so agent call sites
    // state intent.
    bound_error_message(message.into())
}

#[cfg(test)]
fn lsp_error_cmd(start: Instant, code: &str, message: &str) -> CommandResult {
    let envelope = RunnerLspResultEnvelope::err(code, message);
    CommandResult {
        exit_code: Some(0),
        stdout: Some(envelope.to_stdout_json()),
        stderr: Some(String::new()),
        duration_ms: Some(start.elapsed().as_millis() as u64),
        error: None,
    }
}
