use super::RunnerCapabilityRequirement::CodingAgentRuns;
use super::ToolVisibility::ModelVisible;
use super::{
    def, model_spec, permission_risk, require_all_scopes, ToolDefinition, PERMISSION_RISK_JOB,
    PERMISSION_RISK_WRITE, TOOL_CATEGORY_CODING_AGENT,
};
use crate::metadata::{
    ToolPathHint::None as NoPath,
    ToolRisk::{JobRun, Read},
    CODING_AGENT_RUN, TOOL_PROVIDER_RUNNER,
};
use crate::registry::input_schemas::{
    coding_agent_cancel_input_schema, coding_agent_observe_input_schema,
    coding_agent_start_input_schema,
};

pub(super) const DEFINITIONS: &[ToolDefinition] = &[
    permission_risk(
        model_spec(
            require_all_scopes(
                def(
                    "coding_agent_start",
                    super::ToolAuditPolicy::typed_fields(&[
                        super::ToolAuditResultField::value("run_id"),
                        super::ToolAuditResultField::value("project"),
                        super::ToolAuditResultField::value("provider_id"),
                        super::ToolAuditResultField::value("state"),
                        super::ToolAuditResultField::value("execution_state"),
                        super::ToolAuditResultField::value("cancel_requested"),
                        super::ToolAuditResultField::pointer("terminal_stop_reason", "/terminal/stop_reason"),
                        super::ToolAuditResultField::pointer("terminal_error_code", "/terminal/error_code"),
                        super::ToolAuditResultField::pointer("terminal_completed_at", "/terminal/completed_at"),
                        super::ToolAuditResultField::value("error_kind"),
                        super::ToolAuditResultField::value("recovery_kind"),
                    ]),
                    ModelVisible,
                    TOOL_CATEGORY_CODING_AGENT,
                    Some(CodingAgentRuns),
                    TOOL_PROVIDER_RUNNER,
                    super::ToolSemanticContract {
                        effect: super::ToolEffect::Execute,
                        risk: JobRun,
                        approval: super::ToolApprovalPolicy::Standard,
                        idempotency: super::ToolIdempotency::Keyed,
                    },
                    Some(CODING_AGENT_RUN),
                    true,
                    NoPath,
                    true,
                    false,
                    super::ToolSessionEvidencePolicy::NONE,
                ),
                &[CODING_AGENT_RUN, webcodex_core::authority::SCOPE_PROJECT_WRITE],
            ),
            "Start one idempotent delegated ACP coding-agent Run on an exact registered Project and logical Runner provider. Autonomous execution may outlive this request; after any uncertain start, reuse the same idempotency key and observe the same Run rather than dispatching a replacement.",
            coding_agent_start_input_schema,
        ),
        PERMISSION_RISK_JOB,
    ),
    model_spec(
        def(
            "coding_agent_observe",
            super::ToolAuditPolicy::typed_semantic(
                super::ToolAuditSemanticResultPolicy::CodingAgentObservation,
            ),
            ModelVisible,
            TOOL_CATEGORY_CODING_AGENT,
            None,
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Observe,
                risk: Read,
                approval: super::ToolApprovalPolicy::None,
                idempotency: super::ToolIdempotency::PureRead,
            },
            Some(CODING_AGENT_RUN),
            false,
            NoPath,
            false,
            false,
            super::ToolSessionEvidencePolicy::NONE,
        ),
        "Observe bounded normalized events and lifecycle for one existing CodingAgentRun. Return the opaque token for only-new follow-ups; history loss/reset is explicit. Observation never starts, retries, or resumes ACP work.",
        coding_agent_observe_input_schema,
    ),
    permission_risk(
        model_spec(
            def(
            "coding_agent_cancel",
            super::ToolAuditPolicy::typed_fields(&[
                super::ToolAuditResultField::value("run_id"),
                super::ToolAuditResultField::value("project"),
                super::ToolAuditResultField::value("provider_id"),
                super::ToolAuditResultField::value("state"),
                super::ToolAuditResultField::value("execution_state"),
                super::ToolAuditResultField::value("cancel_requested"),
                super::ToolAuditResultField::pointer("terminal_stop_reason", "/terminal/stop_reason"),
                super::ToolAuditResultField::pointer("terminal_error_code", "/terminal/error_code"),
                super::ToolAuditResultField::pointer("terminal_completed_at", "/terminal/completed_at"),
                super::ToolAuditResultField::value("error_kind"),
                super::ToolAuditResultField::value("recovery_kind"),
            ]),
            ModelVisible,
            TOOL_CATEGORY_CODING_AGENT,
            None,
            TOOL_PROVIDER_RUNNER,
            // Cancel is Run lifecycle control but deliberately not a second
            // WebCodex PermissionEvaluator decision after start admission.
            super::ToolSemanticContract {
                effect: super::ToolEffect::Mutate,
                risk: super::ToolRisk::RunControl,
                approval: super::ToolApprovalPolicy::InheritFromStart,
                idempotency: super::ToolIdempotency::DesiredState,
            },
            Some(CODING_AGENT_RUN),
            false,
            NoPath,
            true,
            false,
            super::ToolSessionEvidencePolicy::NONE,
            ),
            "Request cancellation of one existing CodingAgentRun. This does not grant permission, retry a prompt, or create a replacement Run; observe the same run_id for authoritative terminal state.",
            coding_agent_cancel_input_schema,
        ),
        PERMISSION_RISK_WRITE,
    ),
];
