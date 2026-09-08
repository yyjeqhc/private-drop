use super::ToolVisibility::ModelHidden;
use super::{context_recovery_only, def, ToolDefinition, TOOL_CATEGORY_RUNTIME};
use crate::metadata::{ToolPathHint::None as NoPath, ToolRisk::Read, ADMIN, TOOL_PROVIDER_CONTROL};

/// Operator-only forensic diagnostics. The tool is kernel-known so the shared
/// typed dispatcher can enforce its contract, but it is projected only by a
/// capable Stateless MCP 2026 operator surface.
pub(super) const DEFINITIONS: &[ToolDefinition] = &[context_recovery_only(def(
    "read_tool_trace",
    super::ToolAuditPolicy::TYPED_CANONICAL,
    ModelHidden,
    TOOL_CATEGORY_RUNTIME,
    None,
    TOOL_PROVIDER_CONTROL,
    super::ToolSemanticContract {
        effect: super::ToolEffect::Observe,
        risk: Read,
        approval: super::ToolApprovalPolicy::None,
        idempotency: super::ToolIdempotency::PureRead,
    },
    Some(ADMIN),
    false,
    NoPath,
    false,
    false,
))];
