use super::RunnerCapabilityRequirement::{FileRead, Shell};
use super::ToolVisibility::ModelVisible;
use super::{
    adaptive_runtime_direct, context_recovery_only, def, model_spec, ToolDefinition,
    TOOL_CATEGORY_FILE, TOOL_CATEGORY_PROJECT,
};
use crate::audit_policy::*;
use crate::metadata::{
    ToolPathHint::{None as NoPath, SinglePath},
    ToolRisk::Read,
    PROJECT_READ, TOOL_PROVIDER_RUNNER,
};
use crate::registry::input_schemas::{
    list_project_files_input_schema, list_project_tracked_files_input_schema,
    project_overview_input_schema, read_file_input_schema, read_files_input_schema,
    search_project_text_input_schema, search_project_texts_input_schema,
};

pub(super) const SEARCH_DEFINITIONS: &[ToolDefinition] = &[
    context_recovery_only(model_spec(
        def(
            "project_overview",
            ToolAuditPolicy {
                context: AuditContextPolicy::Omit,
                execution: AuditExecutionPolicy::Omit,
                request: AuditRequestPolicy {
                    fields: &[
                        AuditField::new("project", "project", AuditValue::Copy),
                        AuditField::new("path", "path", AuditValue::Copy),
                        AuditField::new("max_depth", "max_depth", AuditValue::Copy),
                        AuditField::new("limit", "limit", AuditValue::Copy),
                    ],
                    transform: AuditTransform::Fields,
                    typed: AuditTypedPolicy::Same,
                },
                result: AuditResultPolicy::SessionEvidence,
            },
            ModelVisible,
            TOOL_CATEGORY_PROJECT,
            Some(FileRead),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Observe,
                risk: Read,
                approval: super::ToolApprovalPolicy::None,
                idempotency: super::ToolIdempotency::PureRead,
            },
            Some(PROJECT_READ),
            true,
            NoPath,
            false,
            false,
        ),
        "Deterministic, bounded, metadata-only overview of an unfamiliar project: conventional project types, manifests, key files, roots, and direct children. Reads no file contents, uses no LLM, and is not semantic/LSP analysis; use read_file for contents.",
        project_overview_input_schema,
    )),
    context_recovery_only(model_spec(
        def(
            "list_project_files",
            ToolAuditPolicy {
                context: AuditContextPolicy::Omit,
                execution: AuditExecutionPolicy::Omit,
                request: AuditRequestPolicy {
                    fields: &[
                        AuditField::new("project", "project", AuditValue::Copy),
                        AuditField::new("path", "path", AuditValue::Copy),
                        AuditField::new("limit", "limit", AuditValue::Copy),
                    ],
                    transform: AuditTransform::Fields,
                    typed: AuditTypedPolicy::Same,
                },
                result: AuditResultPolicy::SessionEvidence,
            },
            ModelVisible,
            TOOL_CATEGORY_FILE,
            Some(FileRead),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Observe,
                risk: Read,
                approval: super::ToolApprovalPolicy::None,
                idempotency: super::ToolIdempotency::PureRead,
            },
            Some(PROJECT_READ),
            true,
            NoPath,
            false,
            false,
        ),
        "List files in a Runner-registered project directory (bounded, read-only). Returns project-relative paths plus a file/dir kind. Routed to the owning registered Runner; the server never reads the Runner project path directly.",
        list_project_files_input_schema,
    )),
    context_recovery_only(model_spec(
        def(
            "list_project_tracked_files",
            ToolAuditPolicy {
                context: AuditContextPolicy::Omit,
                execution: AuditExecutionPolicy::Omit,
                request: AuditRequestPolicy {
                    fields: &[
                        AuditField::new("project", "project", AuditValue::Copy),
                        AuditField::new("path", "path", AuditValue::Copy),
                        AuditField::new("globs", "globs", AuditValue::Copy),
                        AuditField::new("depth", "depth", AuditValue::Copy),
                        AuditField::new("limit", "limit", AuditValue::Copy),
                        AuditField::new("offset", "offset", AuditValue::Copy),
                        AuditField::new("session_id", "session_id", AuditValue::Copy),
                    ],
                    transform: AuditTransform::Fields,
                    typed: AuditTypedPolicy::Omit,
                },
                result: AuditResultPolicy::SessionEvidence,
            },
            ModelVisible,
            TOOL_CATEGORY_FILE,
            // Runs `git ls-files` on the Runner, so the shell capability is what
            // the Runner must actually hold — not FileRead's directory op.
            Some(Shell),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Observe,
                risk: Read,
                approval: super::ToolApprovalPolicy::None,
                idempotency: super::ToolIdempotency::PureRead,
            },
            Some(PROJECT_READ),
            true,
            NoPath,
            false,
            false,
        ),
        "Default discovery tool: what files does this project contain? Lists Git-tracked paths in one bounded call, so ignored directories like .venv and target never appear. Supports globs, a scope, and paging; a project too large to list file by file rolls up to the deepest directory depth that fits.",
        list_project_tracked_files_input_schema,
    )),
    context_recovery_only(model_spec(
        def(
            "search_project_text",
            ToolAuditPolicy {
                context: AuditContextPolicy::Omit,
                execution: AuditExecutionPolicy::Omit,
                request: AuditRequestPolicy {
                    fields: &[
                        AuditField::new("project", "project", AuditValue::Copy),
                        AuditField::new("path", "path", AuditValue::Copy),
                        AuditField::new("limit", "limit", AuditValue::Copy),
                        AuditField::new("context_before", "context_before", AuditValue::Copy),
                        AuditField::new("context_after", "context_after", AuditValue::Copy),
                        AuditField::new("result_mode", "result_mode", AuditValue::Copy),
                        AuditField::new("timeout_secs", "timeout_secs", AuditValue::Copy),
                        AuditField::new("include_glob_count", "include_globs", AuditValue::Count),
                        AuditField::new("exclude_glob_count", "exclude_globs", AuditValue::Count),
                        AuditField::new("pattern_present", "pattern", AuditValue::KeyPresent),
                    ],
                    transform: AuditTransform::Fields,
                    typed: AuditTypedPolicy::Overrides {
                        fields: &[AuditField::new(
                            "pattern_present",
                            "pattern",
                            AuditValue::Present,
                        )],
                        omit: &[],
                    },
                },
                result: AuditResultPolicy::SessionEvidence,
            },
            ModelVisible,
            TOOL_CATEGORY_FILE,
            Some(Shell),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Observe,
                risk: Read,
                approval: super::ToolApprovalPolicy::None,
                idempotency: super::ToolIdempotency::PureRead,
            },
            Some(PROJECT_READ),
            true,
            NoPath,
            false,
            false,
        ),
        "Default inspect/search tool for project text. Uses rg-first with grep fallback. Regex is default; prefer pattern_mode=literal for exact identifiers, snippets, and paths, and request context explicitly when needed. Supports matches/files_with_matches/count. A truncated single search has no safe match cursor: refine the query/path/globs/mode/limit instead of inventing an offset. Failure and fallback diagnostics remain explicit.",
        search_project_text_input_schema,
    )),
    adaptive_runtime_direct(
        context_recovery_only(model_spec(
            def(
                "search_project_texts",
                ToolAuditPolicy {
                    context: AuditContextPolicy::Omit,
                    execution: AuditExecutionPolicy::Omit,
                    request: AuditRequestPolicy {
                        fields: &[
                            AuditField::new("project", "project", AuditValue::Copy),
                            AuditField::new("query_count", "queries", AuditValue::Count),
                            AuditField::new("patterns_present", "queries", AuditValue::AnyPattern),
                        ],
                        transform: AuditTransform::Fields,
                        typed: AuditTypedPolicy::Same,
                    },
                    result: AuditResultPolicy::SessionEvidence,
                },
                ModelVisible,
                TOOL_CATEGORY_FILE,
                Some(Shell),
                TOOL_PROVIDER_RUNNER,
                super::ToolSemanticContract {
                    effect: super::ToolEffect::Observe,
                    risk: Read,
                    approval: super::ToolApprovalPolicy::None,
                    idempotency: super::ToolIdempotency::PureRead,
                },
                Some(PROJECT_READ),
                true,
                NoPath,
                false,
                false,
            ),
            "Run 1 to 8 independent project-text searches with isolated failures and at most two Runner requests in flight. Each query defaults to regex; prefer pattern_mode=literal for identifiers, snippets, paths, and exact text, and request context explicitly. Batch continuation is whole-query via authoritative next_index; an individual truncated query has no safe match cursor and should be refined instead.",
            search_project_texts_input_schema,
        )),
        40,
    ),
];

pub(super) const READ_DEFINITIONS: &[ToolDefinition] = &[
    context_recovery_only(model_spec(
        def(
            "read_file",
            ToolAuditPolicy {
                context: AuditContextPolicy::Omit,
                execution: AuditExecutionPolicy::Omit,
                request: AuditRequestPolicy {
                    fields: &[
                        AuditField::new("project", "project", AuditValue::Copy),
                        AuditField::new("path", "path", AuditValue::Copy),
                        AuditField::new("start_line", "start_line", AuditValue::Copy),
                        AuditField::new("limit", "limit", AuditValue::Copy),
                        AuditField::new("with_line_numbers", "with_line_numbers", AuditValue::Copy),
                    ],
                    transform: AuditTransform::Fields,
                    typed: AuditTypedPolicy::Same,
                },
                result: AuditResultPolicy::SessionEvidence,
            },
            ModelVisible,
            TOOL_CATEGORY_FILE,
            Some(FileRead),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Observe,
                risk: Read,
                approval: super::ToolApprovalPolicy::None,
                idempotency: super::ToolIdempotency::PureRead,
            },
            Some(PROJECT_READ),
            true,
            SinglePath,
            false,
            false,
        ),
        "Default inspect tool for targeted source reading. Bounded UTF-8 range read with full-file sha256. Partial success returns a deterministic positional read_range continuation whose reusable read_file call binds the exact resolved Project id and preserves an explicitly supplied business session_id; shorthand is never replayed. The range cursor is not snapshot-stable, so compare the next full-file sha256 before treating ranges as one unchanged source. Line numbers only change text. Oversized ranges fail range_too_large: shrink limit or narrow the range.",
        read_file_input_schema,
    )),
    adaptive_runtime_direct(
        context_recovery_only(model_spec(
            def(
                "read_files",
                ToolAuditPolicy {
                    context: AuditContextPolicy::Omit,
                    execution: AuditExecutionPolicy::Omit,
                    request: AuditRequestPolicy {
                        fields: &[
                            AuditField::new("project", "project", AuditValue::Copy),
                            AuditField::new("items", "items", AuditValue::Copy),
                            AuditField::new("with_line_numbers", "with_line_numbers", AuditValue::Copy),
                        ],
                        transform: AuditTransform::Fields,
                        typed: AuditTypedPolicy::Same,
                    },
                    result: AuditResultPolicy::SessionEvidence,
                },
                ModelVisible,
                TOOL_CATEGORY_FILE,
                Some(FileRead),
                TOOL_PROVIDER_RUNNER,
                super::ToolSemanticContract {
                    effect: super::ToolEffect::Observe,
                    risk: Read,
                    approval: super::ToolApprovalPolicy::None,
                    idempotency: super::ToolIdempotency::PureRead,
                },
                Some(PROJECT_READ),
                true,
                NoPath,
                false,
                false,
            ),
            "Batch inspect tool for multiple known file ranges; use read_file for one targeted range. Reads 1 to 8 UTF-8 ranges in request order with isolated failures. Partial items return positional read_range continuations; recovery binds the exact resolved Project id and preserves an explicit business session_id, so shorthand is never replayed. Compare sha256 before joining ranges because reads are not snapshot-stable. Budget omission returns batch_items with remaining original items; next_index is evidence, not a read_files input. If no part of the first item fits, increase_result_budget suggests bounded max_result_bytes; zero progress at the hard cap exposes no fake continuation. Complete a current partial item before later batch recovery. Primary batch budget defaults to ~64 KiB, capped at 256 KiB; Session overlays remain bounded.",
            read_files_input_schema,
        )),
        50,
    ),
];
