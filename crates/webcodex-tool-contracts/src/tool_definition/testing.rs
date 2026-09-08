use super::RunnerCapabilityRequirement::{OwnerOnly, Shell};
use super::ToolVisibility::ModelVisible;
use super::{
    adaptive_runtime_direct, captures_validation_output, def, model_spec, ToolDefinition,
    TOOL_CATEGORY_VALIDATION,
};
use crate::metadata::{
    ToolPathHint::None as NoPath, ToolRisk::JobRun, JOB_RUN, TOOL_PROVIDER_RUNNER,
};
use crate::registry::input_schemas::{
    cargo_check_input_schema, cargo_fmt_input_schema, cargo_test_input_schema, go_test_input_schema,
};

pub(super) const DEFINITIONS: &[ToolDefinition] = &[
    captures_validation_output(model_spec(
        def(
            "cargo_fmt",
            super::ToolAuditPolicy::TYPED_CANONICAL
                .execution(super::ToolAuditExecutionPolicy::TEXT),
            ModelVisible,
            TOOL_CATEGORY_VALIDATION,
            Some(Shell),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Execute,
                risk: JobRun,
                approval: super::ToolApprovalPolicy::Standard,
                idempotency: super::ToolIdempotency::NonIdempotent,
            },
            Some(JOB_RUN),
            true,
            NoPath,
            true,
            false,
            super::ToolSessionEvidencePolicy::NONE.validation_identity(super::ToolValidationIdentityKind::CargoFmt),
        ),
        "Run cargo fmt. With check=true it is read-only validation; optional sync_wait_secs shortens only the synchronous grace before the same execution is returned as a Job. Mutating format stays synchronous and rejects sync_wait_secs.",
        cargo_fmt_input_schema,
    )),
    adaptive_runtime_direct(
        captures_validation_output(model_spec(
            def(
                "cargo_check",
                super::ToolAuditPolicy::TYPED_CANONICAL
                    .execution(super::ToolAuditExecutionPolicy::TEXT),
                ModelVisible,
                TOOL_CATEGORY_VALIDATION,
                Some(Shell),
                TOOL_PROVIDER_RUNNER,
                super::ToolSemanticContract {
                    effect: super::ToolEffect::Execute,
                    risk: JobRun,
                    approval: super::ToolApprovalPolicy::Standard,
                    idempotency: super::ToolIdempotency::NonIdempotent,
                },
                Some(JOB_RUN),
                true,
                NoPath,
                false,
                false,
                super::ToolSessionEvidencePolicy::NONE.validation_identity(super::ToolValidationIdentityKind::CargoCheck),
            ),
            "Preferred structured cargo check (default --all-targets). Supports scoped flags without shell interpolation; optional sync_wait_secs controls only the synchronous grace before the same execution is returned as a Job, never total timeout or retry.",
            cargo_check_input_schema,
        )),
        90,
    ),
    adaptive_runtime_direct(
        captures_validation_output(model_spec(
            def(
                "cargo_test",
                super::ToolAuditPolicy::TYPED_CANONICAL
                    .execution(super::ToolAuditExecutionPolicy::TEST_ASSERTIONS),
                ModelVisible,
                TOOL_CATEGORY_VALIDATION,
                Some(Shell),
                TOOL_PROVIDER_RUNNER,
                super::ToolSemanticContract {
                    effect: super::ToolEffect::Execute,
                    risk: JobRun,
                    approval: super::ToolApprovalPolicy::Standard,
                    idempotency: super::ToolIdempotency::NonIdempotent,
                },
                Some(JOB_RUN),
                true,
                NoPath,
                false,
                false,
                super::ToolSessionEvidencePolicy::NONE.validation_identity(super::ToolValidationIdentityKind::CargoTest),
            ),
            "Preferred structured cargo test with scoped args and bounded output. Normal execution requires non-zero executed-test evidence; explicit require_tests=false opts out when no min_tests minimum is requested, while require_tests=true/min_tests enforce a proven minimum. no_run=true is compile-only and does not require executed-test-count proof. Optional sync_wait_secs controls only synchronous grace before the same execution Job is returned; it never changes total timeout, test proof, or retry semantics.",
            cargo_test_input_schema,
        )),
        100,
    ),
    captures_validation_output(model_spec(
            def(
                "go_test",
                super::ToolAuditPolicy::TYPED_CANONICAL
                    .execution(super::ToolAuditExecutionPolicy::TEST_COUNTS),
                ModelVisible,
                TOOL_CATEGORY_VALIDATION,
                Some(OwnerOnly),
                TOOL_PROVIDER_RUNNER,
                super::ToolSemanticContract {
                    effect: super::ToolEffect::Execute,
                    risk: JobRun,
                    approval: super::ToolApprovalPolicy::Standard,
                    idempotency: super::ToolIdempotency::NonIdempotent,
                },
                Some(JOB_RUN),
                true,
                NoPath,
                false,
                false,
                super::ToolSessionEvidencePolicy::NONE.validation_identity(super::ToolValidationIdentityKind::GoTest),
            ),
            "Preferred structured go test -json (default ./...) with bounded package scopes. Requires Runner Go JSON validation support; optional sync_wait_secs controls only synchronous grace before the same execution is returned as a Job, never total timeout or retry.",
            go_test_input_schema,
    )),
];
