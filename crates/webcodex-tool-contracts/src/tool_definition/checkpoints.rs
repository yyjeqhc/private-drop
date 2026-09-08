use super::RunnerCapabilityRequirement::{FileRead, FileWrite, OwnerOnly};
use super::ToolVisibility::ModelVisible;
use super::{
    def, git_like, model_spec, permission_risk, ToolDefinition, PERMISSION_RISK_PATCH,
    TOOL_CATEGORY_CHECKPOINT,
};
use crate::metadata::{
    ToolPathHint::{None as NoPath, Patch},
    ToolRisk::{ProjectWrite, Read},
    PROJECT_READ, PROJECT_WRITE, TOOL_PROVIDER_NATIVE,
};
use crate::registry::input_schemas::{
    checkpoint_create_input_schema, checkpoint_delete_input_schema, checkpoint_list_input_schema,
    checkpoint_restore_input_schema, checkpoint_show_input_schema,
};

pub(super) const DEFINITIONS: &[ToolDefinition] = &[
    git_like(model_spec(
        def(
            "workspace_checkpoint_create",
            super::ToolAuditPolicy::TYPED_CANONICAL,
            ModelVisible,
            TOOL_CATEGORY_CHECKPOINT,
            Some(FileRead),
            TOOL_PROVIDER_NATIVE,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Mutate,
                risk: super::ToolRisk::CheckpointManage,
                approval: super::ToolApprovalPolicy::None,
                idempotency: super::ToolIdempotency::NonIdempotent,
            },
            Some(PROJECT_READ),
            true,
            NoPath,
            false,
            false,
        ),
        "Create a bounded workspace checkpoint outside the project worktree. Captures HEAD, status, text diffs, and optional small untracked text files.",
        checkpoint_create_input_schema,
    )),
    model_spec(
        def(
            "workspace_checkpoint_list",
            super::ToolAuditPolicy::TYPED_CANONICAL,
            ModelVisible,
            TOOL_CATEGORY_CHECKPOINT,
            Some(OwnerOnly),
            TOOL_PROVIDER_NATIVE,
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
        "List checkpoint metadata for a project without returning full diffs or saved file content.",
        checkpoint_list_input_schema,
    ),
    model_spec(
        def(
            "workspace_checkpoint_show",
            super::ToolAuditPolicy::TYPED_CANONICAL,
            ModelVisible,
            TOOL_CATEGORY_CHECKPOINT,
            Some(OwnerOnly),
            TOOL_PROVIDER_NATIVE,
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
        "Show bounded checkpoint metadata, file list, skipped files, and optional diff stat. Does not return full diff/content by default.",
        checkpoint_show_input_schema,
    ),
    git_like(permission_risk(
        model_spec(
            def(
            "workspace_checkpoint_restore",
            super::ToolAuditPolicy::TYPED_CANONICAL,
            ModelVisible,
            TOOL_CATEGORY_CHECKPOINT,
            Some(FileWrite),
            TOOL_PROVIDER_NATIVE,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Mutate,
                risk: ProjectWrite,
                approval: super::ToolApprovalPolicy::Standard,
                idempotency: super::ToolIdempotency::NonIdempotent,
            },
            Some(PROJECT_WRITE),
            true,
            Patch,
            true,
            false,
            ),
            "Restore a checkpoint after confirm=true. Requires matching HEAD and refuses unsafe current state rather than half-restoring.",
            checkpoint_restore_input_schema,
        ),
        PERMISSION_RISK_PATCH,
    )),
    model_spec(
        def(
            "workspace_checkpoint_delete",
            super::ToolAuditPolicy::TYPED_CANONICAL,
            ModelVisible,
            TOOL_CATEGORY_CHECKPOINT,
            Some(OwnerOnly),
            TOOL_PROVIDER_NATIVE,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Mutate,
                risk: ProjectWrite,
                approval: super::ToolApprovalPolicy::Standard,
                idempotency: super::ToolIdempotency::NonIdempotent,
            },
            Some(PROJECT_WRITE),
            true,
            NoPath,
            true,
            false,
        ),
        "Delete one checkpoint JSON file after confirm=true. Does not touch the project worktree.",
        checkpoint_delete_input_schema,
    ),
];
