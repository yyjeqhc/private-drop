use super::RunnerCapabilityRequirement::{ApplyPatch, Shell};
use super::ToolVisibility::ModelVisible;
use super::{
    def, model_spec, permission_risk, ToolDefinition, PERMISSION_RISK_PATCH, TOOL_CATEGORY_PATCH,
};
use crate::metadata::{
    ToolPathHint::Patch, ToolRisk::ProjectWrite, PROJECT_WRITE, TOOL_PROVIDER_RUNNER,
};
use crate::registry::input_schemas::{apply_patch_input_schema, apply_unified_diff_input_schema};

pub(super) const DEFINITIONS: &[ToolDefinition] = &[
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
            super::ToolSessionEvidencePolicy::NONE.changed_paths(super::ToolChangedPathEvidence::ResultField("changed_paths")),
            ),
            "Contextual patch path for guarded exact edits when contextual or multi-hunk form is clearer; line count alone is not a reason to patch. Prefer read_files -> apply_text_edits with current SHA. Repetitive targets need stable unique context (function/impl/type/test/module), not repeated lines/short fragments. Transactional: SHA rechecks, rollback, dry_run. matching_mode=unique default. matching_mode_rejected: never weaken guard or switch to first_match; reread, use apply_text_edits if easy, else bounded read_files recovery. Patch retry preserves requested guard: unique stays unique; matching_mode=exact_unique stays exact_unique—never relax the stale-context/concurrency fence. context_mismatch: reread and regenerate from current source. Keep multiple chunks per file; duplicate file operations reject. first_match is compatibility, not recovery. outcome_unknown: inspect workspace.",
            apply_patch_input_schema,
        ),
        PERMISSION_RISK_PATCH,
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
                super::ToolSessionEvidencePolicy::NONE.changed_paths(super::ToolChangedPathEvidence::ResultField("affected_files")),
            ),
            "External raw unified-diff mutation path. Use only when input is already a standard unified diff; ordinary model-generated edits should use read_files followed by apply_text_edits, while contextual or large patch-shaped changes use apply_patch. Performs bounded preflight before applying and never needs a separate validation call. Shell heredocs and Codex *** Begin Patch wrappers are rejected with recovery metadata.",
            apply_unified_diff_input_schema,
        ),
        PERMISSION_RISK_PATCH,
    ),
];
