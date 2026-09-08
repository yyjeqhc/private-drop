use super::RunnerCapabilityRequirement::{GitOrShell, Shell, StructuredProcess};
use super::ToolVisibility::ModelVisible;
use super::{
    adaptive_runtime_direct, context_recovery_only, def, git_like, model_spec, ToolDefinition,
    TOOL_CATEGORY_CLEANUP,
};
use crate::metadata::{
    ToolPathHint::{None as NoPath, PathList},
    ToolRisk::{ProjectWrite, Read},
    PROJECT_READ, PROJECT_WRITE, TOOL_PROVIDER_RUNNER,
};
use crate::registry::input_schemas::{
    delete_project_files_input_schema, discard_untracked_input_schema,
    git_restore_paths_input_schema, workspace_hygiene_check_input_schema,
};

pub(super) const DEFINITIONS: &[ToolDefinition] = &[adaptive_runtime_direct(
    context_recovery_only(model_spec(
        def(
            "workspace_hygiene_check",
            super::ToolAuditPolicy::TYPED_CANONICAL,
            ModelVisible,
            TOOL_CATEGORY_CLEANUP,
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
        "Default pre-final workspace hygiene review; read-only. Detects dirty worktree, untracked temp/smoke files, cache dirs, secret-like names, and large untracked files before validation or handoff. Never reads file contents.",
        workspace_hygiene_check_input_schema,
    )),
    140,
)];

pub(super) const CLEANUP_DEFINITIONS: &[ToolDefinition] = &[
    model_spec(
        def(
            "delete_project_files",
            super::ToolAuditPolicy::TYPED_CANONICAL,
            ModelVisible,
            TOOL_CATEGORY_CLEANUP,
            Some(Shell),
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
            true,
            false,
        ),
        "Delete selected project-relative files only; safer than arbitrary rm for cleanup.",
        delete_project_files_input_schema,
    ),
    git_like(model_spec(
        def(
            "git_restore_paths",
            super::ToolAuditPolicy::TYPED_CANONICAL,
            ModelVisible,
            TOOL_CATEGORY_CLEANUP,
            Some(StructuredProcess),
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
            true,
            false,
        ),
        "Restore selected tracked paths with git restore; does not remove untracked files.",
        git_restore_paths_input_schema,
    )),
    git_like(model_spec(
        def(
            "discard_untracked",
            super::ToolAuditPolicy::TYPED_CANONICAL,
            ModelVisible,
            TOOL_CATEGORY_CLEANUP,
            Some(StructuredProcess),
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
            true,
            false,
        ),
        "Discard selected untracked files with git clean -f -- <paths>.",
        discard_untracked_input_schema,
    )),
];
