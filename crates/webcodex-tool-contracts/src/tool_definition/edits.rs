use super::RunnerCapabilityRequirement::FileWrite;
use super::ToolVisibility::ModelVisible;
use super::{
    adaptive_runtime_direct, def, model_spec, permission_risk, ToolDefinition,
    PERMISSION_RISK_WRITE, TOOL_CATEGORY_EDIT,
};
use crate::metadata::{
    ToolPathHint::{PathList, SinglePath},
    ToolRisk::ProjectWrite,
    PROJECT_WRITE, TOOL_PROVIDER_RUNNER,
};
use crate::registry::input_schemas::{
    apply_text_edits_input_schema, write_project_file_input_schema,
};

pub(super) const DEFINITIONS: &[ToolDefinition] = &[
    permission_risk(
        model_spec(
            def(
            "write_project_file",
            ModelVisible,
            TOOL_CATEGORY_EDIT,
            Some(FileWrite),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Mutate,
                risk: ProjectWrite,
                approval: super::ToolApprovalPolicy::Standard,
                idempotency: super::ToolIdempotency::NonIdempotent,
            },
            Some(PROJECT_WRITE),
            true,
            SinglePath,
            true,
            false,
            ),
            "Create new files or intentional whole-file rewrites. Existing-file overwrite requires the exact current expected_sha256. For ordinary model-generated changes after read_file/read_files, prefer apply_text_edits with the returned current SHA; use apply_patch when a contextual or large multi-hunk patch is clearer. Inspect current content and worktree changes before replacing a file.",
            write_project_file_input_schema,
        ),
        PERMISSION_RISK_WRITE,
    ),
    adaptive_runtime_direct(
        permission_risk(
            model_spec(
                def(
                "apply_text_edits",
                ModelVisible,
                TOOL_CATEGORY_EDIT,
                Some(FileWrite),
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
                "Default guarded edit path after read_file/read_files for ordinary model-generated changes on the current worktree. Transactional and SHA-guarded: pass each existing file's current read SHA as expected_sha256; exact matches are unique by default, occurrence remains global source order, and optional line_scope fences matches. Supports transactional multi-file edits. Use apply_patch when a contextual or large multi-hunk patch is clearer; use apply_unified_diff only for an external raw diff.",
                apply_text_edits_input_schema,
            ),
            PERMISSION_RISK_WRITE,
        ),
        60,
    ),
];
