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
                super::ToolAuditPolicy::TYPED_CANONICAL,
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
                "Contextual patch path for model-generated changes awkward as guarded exact edits, especially large or multi-hunk rewrites. Prefer read_file/read_files -> apply_text_edits when current text and SHA are available. Transactional with SHA rechecks, rollback, dry_run. matching_mode=unique tolerates bounded whitespace/Unicode drift but writes only to one target; pure additions need a unique anchor. For repetitive targets add a stable parent/function/test/module anchor. matching_mode=exact_unique is only for a stale-context fence after rereading exact source; never relax it after rejection. matching_mode=first_match is permissive compatibility. Keep multiple chunks for one file in one Update File; duplicate file operations reject. Ambiguity may return read_files recovery. outcome_unknown requires workspace inspection before retry. apply_unified_diff is only for external diffs.",
                apply_patch_input_schema,
            ),
            PERMISSION_RISK_PATCH,
        ),
        65,
    ),
    permission_risk(
        model_spec(
            def(
                "apply_unified_diff",
                super::ToolAuditPolicy::TYPED_CANONICAL,
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
            "External raw unified-diff mutation path. Use only when input is already a standard unified diff; ordinary model-generated edits should use read_file/read_files followed by apply_text_edits, while contextual or large patch-shaped changes use apply_patch. Performs bounded preflight before applying and never needs a separate validation call. Shell heredocs and Codex *** Begin Patch wrappers are rejected with recovery metadata.",
            apply_unified_diff_input_schema,
        ),
        PERMISSION_RISK_PATCH,
    ),
];
