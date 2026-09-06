use super::RunnerCapabilityRequirement::{ApplyPatch, Shell};
use super::ToolVisibility::ModelVisible;
use super::{
    adaptive_runtime_direct, def, model_spec, permission_risk, ToolDefinition,
    PERMISSION_RISK_PATCH, TOOL_CATEGORY_PATCH,
};
use crate::metadata::{
    ToolPathHint::Patch, ToolRisk::ProjectWrite, PROJECT_WRITE, TOOL_PROVIDER_RUNNER,
};
use crate::registry::input_schemas::{apply_patch_input_schema, apply_unified_diff_input_schema};

pub(super) const DEFINITIONS: &[ToolDefinition] = &[
    adaptive_runtime_direct(
        permission_risk(
            model_spec(
                def(
                "apply_patch",
                ModelVisible,
                TOOL_CATEGORY_PATCH,
                Some(ApplyPatch),
                TOOL_PROVIDER_RUNNER,
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
                "Primary model edit path for contextual/multi-file Codex patches. Transactional with SHA rechecks, rollback, dry_run. matching_mode=unique (default) allows bounded whitespace/Unicode drift but writes only when final target is unique; repeated `@@` is okay when old lines still identify one target, while pure additions need a unique anchor. For repetitive targets, add a stable parent/function/test/module anchor. Use matching_mode=exact_unique only after reading exact current source for a stale-context fence; never relax it after rejection. matching_mode=first_match is permissive compatibility only. Put multiple chunks for one file in one Update File; duplicate file operations are rejected. Ambiguity may return equal read_files windows. outcome_unknown requires workspace inspection before retry. Prefer apply_text_edits for small exact edits and unified diff for external diffs.",
                apply_patch_input_schema,
            ),
            PERMISSION_RISK_PATCH,
        ),
        60,
    ),
    permission_risk(
        model_spec(
            def(
                "apply_unified_diff",
                ModelVisible,
                TOOL_CATEGORY_PATCH,
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
                Patch,
                true,
                false,
            ),
            "External raw unified-diff mutation path. Prefer apply_patch for model-generated changes and apply_text_edits for small exact guarded edits. Performs bounded preflight before applying and never needs a separate validation call. Input must be a standard unified diff; shell heredocs and Codex *** Begin Patch wrappers are rejected with recovery metadata.",
            apply_unified_diff_input_schema,
        ),
        PERMISSION_RISK_PATCH,
    ),
];
