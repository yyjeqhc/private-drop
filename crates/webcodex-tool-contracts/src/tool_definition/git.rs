use super::RunnerCapabilityRequirement::GitOrShell;
use super::ToolVisibility::ModelVisible;
use super::{
    adaptive_runtime_direct, change_summary_like, context_recovery_only, def, git_like, model_spec,
    require_all_scopes, ToolDefinition, TOOL_CATEGORY_GIT,
};
use crate::metadata::{
    ToolPathHint::{None as NoPath, PathList},
    ToolRisk::{ProjectWrite, Read},
    JOB_RUN, PROJECT_READ, PROJECT_WRITE, TOOL_PROVIDER_RUNNER,
};
use crate::registry::input_schemas::{
    git_commit_paths_input_schema, git_diff_hunks_input_schema, git_log_input_schema,
    git_review_summary_input_schema, git_status_input_schema, show_changes_input_schema,
};

pub(super) const SUMMARY_DEFINITIONS: &[ToolDefinition] = &[
    adaptive_runtime_direct(
        context_recovery_only(change_summary_like(git_like(model_spec(
            def(
                "git_review_summary",
                super::ToolAuditPolicy::typed_fields(&[
                    super::ToolAuditResultField::value("project"),
                    super::ToolAuditResultField::value("scope"),
                    super::ToolAuditResultField::value("stats"),
                    super::ToolAuditResultField::value("coverage"),
                    super::ToolAuditResultField::value("truncation"),
                    super::ToolAuditResultField::value("deterministic"),
                    super::ToolAuditResultField::value("llm_summary"),
                    super::ToolAuditResultField::value("truncated"),
                    super::ToolAuditResultField::value("reason_code"),
                    super::ToolAuditResultField::array_len("signal_count", "signals"),
                    super::ToolAuditResultField::array_len("file_count", "files"),
                ])
                .drop_null_request_values(),
                ModelVisible,
                TOOL_CATEGORY_GIT,
                Some(GitOrShell),
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
                super::ToolSessionEvidencePolicy::NONE.review(super::ToolReviewEvidence::ReadOnlyInspection),
            ),
            "Deterministic bounded committed-range review map. Use before targeted git_diff_hunks/read_files during branch or PR review. Does not judge correctness and never mutates the repository.",
            git_review_summary_input_schema,
        )))),
        120,
    ),
    adaptive_runtime_direct(
        context_recovery_only(change_summary_like(git_like(model_spec(
            def(
                "show_changes",
                super::ToolAuditPolicy::TYPED_CANONICAL.context(
                    super::ToolAuditContextPolicy::Fields(&[
                        super::ToolAuditResultField::value("clean"),
                        super::ToolAuditResultField::value("branch"),
                        super::ToolAuditResultField::value("head"),
                        super::ToolAuditResultField::value("upstream"),
                        super::ToolAuditResultField::value("ahead"),
                        super::ToolAuditResultField::value("behind"),
                        super::ToolAuditResultField::value("counts"),
                        super::ToolAuditResultField::value("changed_files"),
                    ]),
                ),
                ModelVisible,
                TOOL_CATEGORY_GIT,
                Some(GitOrShell),
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
                super::ToolSessionEvidencePolicy::NONE.review(super::ToolReviewEvidence::WorkspaceReview).diff_review(super::ToolDiffReviewEvidence::ArgumentBool("include_diff")),
            ),
            "Default inspect/review tool before final response. Read-only worktree overview with bounded hunks and compact Session signals; recent Session event history is omitted unless session_event_limit is explicitly positive. If hunks truncate, diff_review_handoff classifies page/line/mixed truncation and provides a parser-ready git_diff_hunks recovery call.",
            show_changes_input_schema,
        )))),
        130,
    ),
];

pub(super) const DETAIL_DEFINITIONS: &[ToolDefinition] = &[
    require_all_scopes(git_like(model_spec(
        def(
            "git_commit_paths",
            super::ToolAuditPolicy::TYPED_CANONICAL.drop_null_request_values(),
            ModelVisible,
            TOOL_CATEGORY_GIT,
            Some(GitOrShell),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Mutate,
                risk: ProjectWrite,
                approval: super::ToolApprovalPolicy::Standard,
                idempotency: super::ToolIdempotency::NonIdempotent,
            },
            Some(PROJECT_WRITE),
            true,
            PathList,
            false,
            false,
            super::ToolSessionEvidencePolicy::NONE,
        ),
        "Commit exactly requested changed file paths with an atomic expected_head fence and isolated temporary index; normal Git clean filters may run under job:run authority, ordinary commit hooks are bypassed so they cannot add unrelated paths, and the tool never pushes.",
        git_commit_paths_input_schema,
    )), &[PROJECT_WRITE, JOB_RUN]),
    context_recovery_only(git_like(model_spec(
        def(
            "git_status",
            super::ToolAuditPolicy::TYPED_CANONICAL
                .context(super::ToolAuditContextPolicy::WorkingTreeStatus),
            ModelVisible,
            TOOL_CATEGORY_GIT,
            Some(GitOrShell),
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
            super::ToolSessionEvidencePolicy::NONE.review(super::ToolReviewEvidence::WorkspaceReview),
        ),
        "Run git status --porcelain for a project.",
        git_status_input_schema,
    ))),
    adaptive_runtime_direct(
        context_recovery_only(change_summary_like(git_like(model_spec(
            def(
                "git_diff_hunks",
                super::ToolAuditPolicy::typed_fields(&[
                    super::ToolAuditResultField::value("project"),
                    super::ToolAuditResultField::value("scope"),
                    super::ToolAuditResultField::value("cached"),
                    super::ToolAuditResultField::value("hunk_count"),
                    super::ToolAuditResultField::value("truncated"),
                    super::ToolAuditResultField::value("truncation_reasons"),
                    super::ToolAuditResultField::value("has_more"),
                    super::ToolAuditResultField::value("exit_code"),
                    super::ToolAuditResultField::value("error_kind"),
                    super::ToolAuditResultField::value("reason_code"),
                    super::ToolAuditResultField::array_len("file_count", "files"),
                ])
                .session_input(super::ToolAuditSessionInputPolicy::OmitTopLevel(&[
                    "continuation",
                ])),
                ModelVisible,
                TOOL_CATEGORY_GIT,
                Some(GitOrShell),
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
                super::ToolSessionEvidencePolicy::NONE.review(super::ToolReviewEvidence::DiffReview).diff_review(super::ToolDiffReviewEvidence::Always),
            ),
            "Targeted/paged diff review for worktree/cached or exact base/head ranges, with paths and scope-bound opaque continuation. max_page_bytes controls the raw producer page (64 KiB default, bounded below ordinary Runner result retention); it is separate from the 512 KiB final model-facing result ceiling. Replay scope and paging inputs unchanged for later records. Continuation only recovers later records; it never reconstructs lines omitted inside the current hunk. When the truncation reason is hunk_line_limit, use larger max_hunk_lines and/or narrower paths when recovery metadata proves that safe. Fixed byte/line ceilings never advertise fake recovery. Read-only.",
            git_diff_hunks_input_schema,
        )))),
        125,
    ),
    context_recovery_only(git_like(model_spec(
        def(
            "git_log",
            super::ToolAuditPolicy::TYPED_CANONICAL.context(
                super::ToolAuditContextPolicy::Fields(&[
                    super::ToolAuditResultField::value("commits"),
                    super::ToolAuditResultField::value("next_skip"),
                    super::ToolAuditResultField::value("truncated"),
                ]),
            ),
            ModelVisible,
            TOOL_CATEGORY_GIT,
            Some(GitOrShell),
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
            super::ToolSessionEvidencePolicy::NONE,
        ),
        "Return bounded structured recent git commit history for a project. When truncated, next_skip is the exact parser-ready offset for the next page when it can advance within the existing 10000 skip bound; null means no safe forward page is available. Retained-tail or malformed source records fail closed; retry with a smaller limit. Offset paging assumes history is unchanged between calls. Does not return commit bodies or modify the worktree.",
        git_log_input_schema,
    ))),
];
