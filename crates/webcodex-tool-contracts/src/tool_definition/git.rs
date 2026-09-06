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
    git_commit_paths_input_schema, git_diff_hunks_input_schema, git_diff_input_schema,
    git_diff_summary_input_schema, git_log_input_schema, git_review_summary_input_schema,
    git_status_input_schema, show_changes_input_schema,
};

pub(super) const SUMMARY_DEFINITIONS: &[ToolDefinition] = &[
    context_recovery_only(change_summary_like(git_like(model_spec(
        def(
            "git_diff_summary",
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
        ),
        "Read-only git diff summary for a project: `git status --porcelain`, `git diff --stat`, and a parsed changed-file list. Does not modify the worktree.",
        git_diff_summary_input_schema,
    )))),
    adaptive_runtime_direct(
        context_recovery_only(change_summary_like(git_like(model_spec(
            def(
                "git_review_summary",
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
            ),
            "Default inspect/review tool before final response. Read-only worktree overview with bounded hunks. If hunks truncate, diff_review_handoff classifies page/line/mixed truncation and provides a parser-ready git_diff_hunks recovery call.",
            show_changes_input_schema,
        )))),
        130,
    ),
];

pub(super) const DETAIL_DEFINITIONS: &[ToolDefinition] = &[
    require_all_scopes(git_like(model_spec(
        def(
            "git_commit_paths",
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
        ),
        "Commit exactly requested changed file paths with an atomic expected_head fence and isolated temporary index; normal Git clean filters may run under job:run authority, ordinary commit hooks are bypassed so they cannot add unrelated paths, and the tool never pushes.",
        git_commit_paths_input_schema,
    )), &[PROJECT_WRITE, JOB_RUN]),
    context_recovery_only(git_like(model_spec(
        def(
            "git_status",
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
        ),
        "Run git status --porcelain for a project.",
        git_status_input_schema,
    ))),
    context_recovery_only(git_like(model_spec(
        def(
            "git_diff",
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
        ),
        "Run git diff for a project, optionally scoped to paths.",
        git_diff_input_schema,
    ))),
    adaptive_runtime_direct(
        context_recovery_only(change_summary_like(git_like(model_spec(
            def(
                "git_diff_hunks",
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
            ),
            "Targeted/paged diff review for worktree/cached or exact base/head ranges, with paths and scope-bound opaque continuation. Replay scope and paging inputs unchanged for later records. Truncated results also include parser-ready recovery; hunk_line_limit needs a fresh call with larger max_hunk_lines and/or narrower paths because continuation cannot recover omitted lines. Read-only.",
            git_diff_hunks_input_schema,
        )))),
        125,
    ),
    context_recovery_only(git_like(model_spec(
        def(
            "git_log",
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
        ),
        "Return bounded structured recent git commit history for a project. Does not return commit bodies or modify the worktree.",
        git_log_input_schema,
    ))),
];
