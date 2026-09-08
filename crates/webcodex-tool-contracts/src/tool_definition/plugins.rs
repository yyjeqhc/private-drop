use super::ToolVisibility::ModelVisible;
use super::{
    adaptive_runtime_direct, def, model_spec, require_any_scopes, ToolDefinition,
    TOOL_CATEGORY_RUNTIME,
};
use crate::audit_policy::*;
use crate::metadata::{
    ToolPathHint::None as NoPath, ToolRisk::RunControl, PLUGIN_INSPECT, PLUGIN_INVOKE,
    PLUGIN_MANAGE, TOOL_PROVIDER_CONTROL,
};
use crate::registry::input_schemas::plugin_tool_input_schema;

const PLUGIN_GATEWAY_SCOPES: &[&str] = &[PLUGIN_INSPECT, PLUGIN_INVOKE, PLUGIN_MANAGE];

pub(super) const DEFINITIONS: &[ToolDefinition] = &[adaptive_runtime_direct(
    require_any_scopes(
        model_spec(
            def(
                "plugin_tool",
                ToolAuditPolicy {
                    request: AuditRequestPolicy {
                        fields: &[],
                        transform: AuditTransform::Fields,
                        typed: AuditTypedPolicy::Omit,
                    },
                    result: AuditResultPolicy::Fields(&[]),
                },
                ModelVisible,
                TOOL_CATEGORY_RUNTIME,
                None,
                TOOL_PROVIDER_CONTROL,
                super::ToolSemanticContract {
                    effect: super::ToolEffect::Execute,
                    risk: RunControl,
                    approval: super::ToolApprovalPolicy::Standard,
                    idempotency: super::ToolIdempotency::NonIdempotent,
                },
                None,
                false,
                NoPath,
                false,
                false,
            ),
            "Stable gateway for Runner-owned native Tool Plugins. Provider tools are never outer WebCodex MCP tools. Discovery begins at an exact caller-visible Runner; describe observes one exact Runner/provider/tool schema and returns an opaque binding; call accepts only binding + arguments, never retargets, relists, reloads, or blindly retries. Gateway visibility requires any Plugin scope, while each action separately enforces plugin:inspect, plugin:invoke, or plugin:manage before provider dispatch.",
            plugin_tool_input_schema,
        ),
        PLUGIN_GATEWAY_SCOPES,
    ),
    26,
)];
