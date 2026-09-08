use super::ToolVisibility::ModelVisible;
use super::{def, model_spec, ToolDefinition, TOOL_CATEGORY_RUNTIME};
use crate::audit_policy::*;
use crate::metadata::{
    ToolPathHint::None as NoPath, ToolRisk::RunControl, SSH_LOCAL, TOOL_PROVIDER_CONTROL,
};
use crate::registry::input_schemas::ssh_resource_input_schema;

pub(super) const DEFINITIONS: &[ToolDefinition] = &[model_spec(
    def(
        "ssh_resource",
        ToolAuditPolicy {
            request: AuditRequestPolicy {
                fields: &[],
                transform: AuditTransform::Fields,
                typed_fields: &[],
                typed_omit: &[],
            },
            result: AuditResultPolicy::Fields(&[]),
        },
        ModelVisible,
        TOOL_CATEGORY_RUNTIME,
        None,
        TOOL_PROVIDER_CONTROL,
        super::ToolSemanticContract {
            effect: super::ToolEffect::Mutate,
            risk: RunControl,
            approval: super::ToolApprovalPolicy::Standard,
            idempotency: super::ToolIdempotency::FencedReplay,
        },
        Some(SSH_LOCAL),
        false,
        NoPath,
        true,
        false,
    ),
    "Discover and manage Runner-local named SSH resources only when PersistentShell needs a durable remote target. Start with action=list on one exact Runner; register/remove require its opaque revision binding. After a registration that requires restart, restart the Runner, list again, bind the active name with update_session_context, then discover open_session_shell/session_shell_exec. For one-shot SSH without persistent state, keep run_process. Targets and authentication details are never returned.",
    ssh_resource_input_schema,
)];
