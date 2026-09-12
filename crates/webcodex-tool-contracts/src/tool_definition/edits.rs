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
            super::ToolAuditPolicy::TYPED_CANONICAL,
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
            super::ToolSessionEvidencePolicy::NONE,
            ),
            "Create new files or intentional whole-file rewrites. Existing-file overwrite requires the exact current expected_sha256. For ordinary model-generated changes after read_files, prefer apply_text_edits with the returned current SHA. Use apply_patch only when contextual or multi-hunk patch form is materially clearer, not merely because many lines change. Inspect current content and worktree changes before replacing a file.",
            write_project_file_input_schema,
        ),
        PERMISSION_RISK_WRITE,
    ),
    adaptive_runtime_direct(
        permission_risk(
            model_spec(
                def(
                "apply_text_edits",
                super::ToolAuditPolicy::TYPED_CANONICAL,
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
                super::ToolSessionEvidencePolicy::NONE,
                ),
                "Canonical default guarded edit path after read_files for ordinary model-generated changes on the current worktree. Transactional and SHA-guarded: pass each existing file's current read SHA as expected_sha256; exact matches are unique by default, occurrence stays global source order, and line_scope optionally fences matches. Supports transactional multi-file edits; empty insert_before/insert_after text is a provable no-op that does not invalidate the batch. Many changed lines alone are not a reason to choose apply_patch. On conflict_recovery.direct_retry_safe=true, use the returned candidate occurrence/range without rereading; reread only when reread_required=true. Use apply_patch only when contextual or large multi-hunk form is materially clearer; external raw diffs use apply_unified_diff.",
                apply_text_edits_input_schema,
            ),
            PERMISSION_RISK_WRITE,
        ),
        60,
    ),
];
