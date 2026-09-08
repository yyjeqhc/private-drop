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
            super::ToolAuditPolicy::TYPED_CANONICAL,
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
        ),
        "Run cargo fmt. With check=true it is read-only validation; a long check continues as the same execution and returns job_id for observation. Mutating format stays synchronous.",
        cargo_fmt_input_schema,
    )),
    adaptive_runtime_direct(
        captures_validation_output(model_spec(
            def(
                "cargo_check",
                super::ToolAuditPolicy::TYPED_CANONICAL,
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
            ),
            "Preferred structured cargo check (default --all-targets). Supports scoped flags without shell interpolation; a long validation continues as the same execution and returns job_id.",
            cargo_check_input_schema,
        )),
        90,
    ),
    adaptive_runtime_direct(
        captures_validation_output(model_spec(
            def(
                "cargo_test",
                super::ToolAuditPolicy::TYPED_CANONICAL,
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
            ),
            "Preferred structured cargo test with scoped args and bounded output. Normal execution requires non-zero executed-test evidence; explicit require_tests=false opts out when no min_tests minimum is requested, while require_tests=true/min_tests enforce a proven minimum. no_run=true is compile-only and does not require executed-test-count proof. Long validation continues as the same execution Job.",
            cargo_test_input_schema,
        )),
        100,
    ),
    captures_validation_output(model_spec(
            def(
                "go_test",
                super::ToolAuditPolicy::TYPED_CANONICAL,
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
            ),
            "Preferred structured go test -json (default ./...) with bounded package scopes. Requires Runner Go JSON validation support; long validation continues as the same execution and returns job_id.",
            go_test_input_schema,
    )),
];
