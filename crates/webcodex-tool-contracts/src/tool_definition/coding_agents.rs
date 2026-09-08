use super::RunnerCapabilityRequirement::CodingAgentRuns;
use super::ToolVisibility::ModelVisible;
use super::{
    def, model_spec, permission_risk, require_all_scopes, ToolDefinition, PERMISSION_RISK_JOB,
    PERMISSION_RISK_WRITE, TOOL_CATEGORY_CODING_AGENT,
};
use crate::audit_policy::*;
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
                    ToolAuditPolicy {
                        request: AuditRequestPolicy {
                            fields: &[
                                AuditField::new("project", "project", AuditValue::Copy),
                                AuditField::new("provider_id", "provider_id", AuditValue::Copy),
                                AuditField::new("timeout_secs", "timeout_secs", AuditValue::Copy),
                                AuditField::new("instruction_bytes", "instruction", AuditValue::Bytes),
                                AuditField::new("config_count", "config", AuditValue::ObjectCount),
                                AuditField::new(
                                    "idempotency_key_present",
                                    "idempotency_key",
                                    AuditValue::StringPresent,
                                ),
                            ],
                            transform: AuditTransform::Fields,
                            typed: AuditTypedPolicy::Same,
                        },
                        result: AuditResultPolicy::Fields(&[
                            AuditField::new("run_id", "run_id", AuditValue::Nullable),
                            AuditField::new("project", "project", AuditValue::Nullable),
                            AuditField::new("provider_id", "provider_id", AuditValue::Nullable),
                            AuditField::new("state", "state", AuditValue::Nullable),
                            AuditField::new("execution_state", "execution_state", AuditValue::Nullable),
                            AuditField::new("cancel_requested", "cancel_requested", AuditValue::Nullable),
                            AuditField::new(
                                "terminal_stop_reason",
                                "/terminal/stop_reason",
                                AuditValue::Nullable,
                            ),
                            AuditField::new(
                                "terminal_error_code",
                                "/terminal/error_code",
                                AuditValue::Nullable,
                            ),
                            AuditField::new(
                                "terminal_completed_at",
                                "/terminal/completed_at",
                                AuditValue::Nullable,
                            ),
                            AuditField::new("error_kind", "error_kind", AuditValue::Nullable),
                            AuditField::new("recovery_kind", "recovery_kind", AuditValue::Nullable),
                        ]),
                    },
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
            ToolAuditPolicy {
                request: AuditRequestPolicy {
                    fields: &[
                        AuditField::new("project", "project", AuditValue::Copy),
                        AuditField::new("run_id", "run_id", AuditValue::Copy),
                        AuditField::new("wait_secs", "wait_secs", AuditValue::Copy),
                        AuditField::new(
                            "token_present",
                            "after_observation_token",
                            AuditValue::StringPresent,
                        ),
                    ],
                    transform: AuditTransform::Fields,
                    typed: AuditTypedPolicy::Overrides { fields: &[], omit: &["project"] },
                },
                result: AuditResultPolicy::CodingEvents(&[
                    AuditField::new("run_id", "run_id", AuditValue::Nullable),
                    AuditField::new("project", "project", AuditValue::Nullable),
                    AuditField::new("provider_id", "provider_id", AuditValue::Nullable),
                    AuditField::new("state", "state", AuditValue::Nullable),
                    AuditField::new("execution_state", "execution_state", AuditValue::Nullable),
                    AuditField::new("has_more", "has_more", AuditValue::Nullable),
                    AuditField::new("history_lost", "history_lost", AuditValue::Nullable),
                    AuditField::new(
                        "first_retained_sequence",
                        "first_retained_sequence",
                        AuditValue::Nullable,
                    ),
                    AuditField::new(
                        "terminal_stop_reason",
                        "/terminal/stop_reason",
                        AuditValue::Nullable,
                    ),
                    AuditField::new(
                        "terminal_error_code",
                        "/terminal/error_code",
                        AuditValue::Nullable,
                    ),
                    AuditField::new(
                        "terminal_completed_at",
                        "/terminal/completed_at",
                        AuditValue::Nullable,
                    ),
                    AuditField::new("recovery_kind", "recovery_kind", AuditValue::Nullable),
                    AuditField::new("error_kind", "error_kind", AuditValue::Nullable),
                ]),
            },
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
        ),
        "Observe bounded normalized events and lifecycle for one existing CodingAgentRun. Return the opaque token for only-new follow-ups; history loss/reset is explicit. Observation never starts, retries, or resumes ACP work.",
        coding_agent_observe_input_schema,
    ),
    permission_risk(
        model_spec(
            def(
            "coding_agent_cancel",
            ToolAuditPolicy {
                request: AuditRequestPolicy {
                    fields: &[
                        AuditField::new("project", "project", AuditValue::Copy),
                        AuditField::new("run_id", "run_id", AuditValue::Copy),
                    ],
                    transform: AuditTransform::Fields,
                    typed: AuditTypedPolicy::Overrides { fields: &[], omit: &["project"] },
                },
                result: AuditResultPolicy::Fields(&[
                    AuditField::new("run_id", "run_id", AuditValue::Nullable),
                    AuditField::new("project", "project", AuditValue::Nullable),
                    AuditField::new("provider_id", "provider_id", AuditValue::Nullable),
                    AuditField::new("state", "state", AuditValue::Nullable),
                    AuditField::new("execution_state", "execution_state", AuditValue::Nullable),
                    AuditField::new("cancel_requested", "cancel_requested", AuditValue::Nullable),
                    AuditField::new(
                        "terminal_stop_reason",
                        "/terminal/stop_reason",
                        AuditValue::Nullable,
                    ),
                    AuditField::new(
                        "terminal_error_code",
                        "/terminal/error_code",
                        AuditValue::Nullable,
                    ),
                    AuditField::new(
                        "terminal_completed_at",
                        "/terminal/completed_at",
                        AuditValue::Nullable,
                    ),
                    AuditField::new("error_kind", "error_kind", AuditValue::Nullable),
                    AuditField::new("recovery_kind", "recovery_kind", AuditValue::Nullable),
                ]),
            },
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
            ),
            "Request cancellation of one existing CodingAgentRun. This does not grant permission, retry a prompt, or create a replacement Run; observe the same run_id for authoritative terminal state.",
            coding_agent_cancel_input_schema,
        ),
        PERMISSION_RISK_WRITE,
    ),
];
