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
                "Contextual patch path for changes awkward as guarded exact edits. Prefer read_file/read_files -> apply_text_edits with current SHA; line count alone is not a reason to patch. Patch when contextual or multi-hunk form is clearer. Repetitive targets need stable unique context: function/impl/type/test/module, not repeated lines/short fragments. Transactional; SHA rechecks, rollback, dry_run. matching_mode=unique default. matching_mode_rejected: never weaken guard or switch to first_match; reread; use apply_text_edits if easy, else bounded read_files recovery, regenerate unique patch. context_mismatch: reread and regenerate from current source. matching_mode=exact_unique only for stale-context/concurrency fence; never relax it. first_match is compatibility, not recovery. Keep multiple chunks per file together; duplicate file operations reject. outcome_unknown requires workspace inspection.",
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
