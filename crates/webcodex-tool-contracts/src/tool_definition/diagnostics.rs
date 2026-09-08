use super::ToolVisibility::ModelHidden;
use super::{context_recovery_only, def, ToolDefinition, TOOL_CATEGORY_RUNTIME};
use crate::audit_policy::*;
use crate::metadata::{ToolPathHint::None as NoPath, ToolRisk::Read, ADMIN, TOOL_PROVIDER_CONTROL};

/// Operator-only forensic diagnostics. The tool is kernel-known so the shared
/// typed dispatcher can enforce its contract, but it is projected only by a
/// capable Stateless MCP 2026 operator surface.
pub(super) const DEFINITIONS: &[ToolDefinition] = &[context_recovery_only(def(
    "read_tool_trace",
    ToolAuditPolicy {
        request: AuditRequestPolicy {
            fields: &[],
            transform: AuditTransform::Fields,
            typed_fields: &[],
            typed_omit: &[],
        },
        result: AuditResultPolicy::Fields(&[
            AuditField::new("trace_ref", "trace_ref", AuditValue::Nullable),
            AuditField::new("trace_mode", "trace_mode", AuditValue::Nullable),
            AuditField::new("payload_count", "payload_count", AuditValue::Nullable),
            AuditField::new("returned_count", "returned_count", AuditValue::Nullable),
            AuditField::new("offset", "offset", AuditValue::Nullable),
            AuditField::new("next_offset", "next_offset", AuditValue::Nullable),
            AuditField::new("payload_index", "payload_index", AuditValue::Nullable),
            AuditField::new("phase", "phase", AuditValue::Nullable),
            AuditField::new("payload_bytes", "payload_bytes", AuditValue::Nullable),
            AuditField::new("payload_sha256", "payload_sha256", AuditValue::Nullable),
            AuditField::new(
                "payload_available",
                "payload_available",
                AuditValue::Nullable,
            ),
            AuditField::new("reason", "reason", AuditValue::Nullable),
            AuditField::new("error_kind", "error_kind", AuditValue::Nullable),
        ]),
    },
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
